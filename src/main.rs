use std::collections::HashSet;
use std::env;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use reqwest::StatusCode;
use reqwest::blocking::Client;
use reqwest::blocking::multipart::{Form, Part};
use reqwest::header::{ACCEPT, ACCEPT_LANGUAGE, USER_AGENT};
use scraper::{Html, Selector};
use serde::Deserialize;
use zip::CompressionMethod;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const DEFAULT_OUTPUT: &str = "chapter.txt";
const BASE_CHAPTER_URL: &str = "https://www.mvlempyr.io/chapter";
const DEFAULT_BOT_OUTPUT_ROOT: &str = "telegram_books";

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 || args.len() > 4 {
        bail!(
            "usage:\n  {} bot [output-root]\n  {} <novel-id> [output-root] [start-chapter]\n  {} <url> [output-file]",
            args[0],
            args[0],
            args[0]
        );
    }

    let client = build_client()?;

    if args[1] == "bot" {
        let output_root = args.get(2).map_or(DEFAULT_BOT_OUTPUT_ROOT, String::as_str);
        run_telegram_bot(Path::new(output_root))?;
        return Ok(());
    }

    if args[1].starts_with("http://") || args[1].starts_with("https://") {
        let output = args.get(2).map_or(DEFAULT_OUTPUT, String::as_str);
        let html = fetch_page(&client, &args[1])?.context("page was not found")?;
        let chapter = parse_chapter(&html)?;
        write_output(Path::new(output), &chapter.text)?;

        println!("wrote {} bytes to {}", chapter.text.len(), output);
        return Ok(());
    }

    let novel_id = &args[1];
    let output_root = args.get(2).map_or(".", String::as_str);
    let start_chapter = args
        .get(3)
        .map(|value| value.parse::<u32>().context("invalid start chapter"))
        .transpose()?
        .unwrap_or(1);

    crawl_novel(&client, novel_id, Path::new(output_root), start_chapter)?;
    Ok(())
}

fn build_client() -> Result<Client> {
    Client::builder()
        .build()
        .context("failed to build HTTP client")
}

fn fetch_page(client: &Client, url: &str) -> Result<Option<String>> {
    let response = client
        .get(url)
        .header(USER_AGENT, "Mozilla/5.0 chapter-extract/0.1")
        .header(ACCEPT, "text/html,application/xhtml+xml")
        .header(ACCEPT_LANGUAGE, "en-US,en;q=0.9")
        .send()
        .with_context(|| format!("failed to fetch {url}"))?;

    if response.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let response = response
        .error_for_status()
        .with_context(|| format!("server returned an error for {url}"))?;

    response
        .text()
        .map(Some)
        .context("failed to read response body")
}

fn crawl_novel(
    client: &Client,
    novel_id: &str,
    output_root: &Path,
    start_chapter: u32,
) -> Result<PathBuf> {
    let existing_dir = find_existing_novel_dir(output_root, novel_id)?;
    let existing_max = existing_dir
        .as_deref()
        .map(highest_existing_chapter)
        .transpose()?
        .unwrap_or(0);
    let mut chapter_number = existing_max.saturating_add(1).max(start_chapter);
    let mut output_dir = existing_dir;
    let mut saved = 0_u32;

    if let Some(dir) = &output_dir {
        println!(
            "resuming in {} from chapter {chapter_number}",
            dir.display()
        );
    }

    loop {
        let url = format!("{BASE_CHAPTER_URL}/{novel_id}-{chapter_number}");
        let Some(html) = fetch_page(client, &url)? else {
            println!("stopped at chapter {chapter_number}: 404 page not found");
            break;
        };

        let chapter = parse_chapter(&html)
            .with_context(|| format!("failed to parse chapter {chapter_number} from {url}"))?;

        let dir =
            output_dir.get_or_insert_with(|| novel_dir(output_root, novel_id, &chapter.novel_name));
        let file_name = format!(
            "{:04} - {}.txt",
            chapter_number,
            sanitize_path_component(&chapter.chapter_name, "chapter")
        );
        let path = dir.join(file_name);

        write_output(&path, &chapter.text)?;
        println!("saved chapter {chapter_number}: {}", path.display());

        saved += 1;
        chapter_number += 1;
    }

    println!("saved {saved} chapters");
    let dir = output_dir.context("no novel output folder was created or found")?;
    let epub_path = build_epub(&dir, novel_id)?;
    println!("wrote epub: {}", epub_path.display());
    Ok(epub_path)
}

#[derive(Debug, Deserialize)]
struct TelegramResponse<T> {
    ok: bool,
    result: T,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

fn run_telegram_bot(output_root: &Path) -> Result<()> {
    let token = env::var("TELEGRAM_BOT_TOKEN")
        .context("set TELEGRAM_BOT_TOKEN to your Telegram bot token")?;
    let telegram_client = build_client()?;
    let site_client = build_client()?;
    let mut offset = None;

    println!("telegram bot is running; command: /novel <novel-id>");

    loop {
        let updates = get_updates(&telegram_client, &token, offset)?;

        for update in updates {
            offset = Some(update.update_id + 1);

            let Some(message) = update.message else {
                continue;
            };
            let Some(text) = message.text else {
                continue;
            };

            if let Err(error) = handle_bot_message(
                &telegram_client,
                &site_client,
                &token,
                output_root,
                message.chat.id,
                &text,
            ) {
                let error_text = format!("error: {error:#}");
                eprintln!("{error_text}");
                let _ = send_message(&telegram_client, &token, message.chat.id, &error_text);
            }
        }
    }
}

fn handle_bot_message(
    telegram_client: &Client,
    site_client: &Client,
    token: &str,
    output_root: &Path,
    chat_id: i64,
    text: &str,
) -> Result<()> {
    let Some(novel_id) = parse_novel_command(text) else {
        if text.starts_with("/start") || text.starts_with("/help") {
            send_message(
                telegram_client,
                token,
                chat_id,
                "Send /novel <novel-id>, for example: /novel 13736",
            )?;
        }
        return Ok(());
    };

    send_message(
        telegram_client,
        token,
        chat_id,
        &format!("working on novel {novel_id}; checking cache and latest chapter"),
    )?;

    let epub_path = crawl_novel(site_client, &novel_id, output_root, 1)?;
    send_document(
        telegram_client,
        token,
        chat_id,
        &epub_path,
        &format!("novel {novel_id}"),
    )?;

    Ok(())
}

fn parse_novel_command(text: &str) -> Option<String> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    if command != "/novel" && !command.starts_with("/novel@") {
        return None;
    }

    let novel_id = parts.next()?.trim();
    if novel_id.chars().all(|character| character.is_ascii_digit()) {
        Some(novel_id.to_string())
    } else {
        None
    }
}

fn get_updates(client: &Client, token: &str, offset: Option<i64>) -> Result<Vec<TelegramUpdate>> {
    let url = telegram_url(token, "getUpdates");
    let mut query = vec![
        ("timeout".to_string(), "30".to_string()),
        ("allowed_updates".to_string(), r#"["message"]"#.to_string()),
    ];

    if let Some(offset) = offset {
        query.push(("offset".to_string(), offset.to_string()));
    }

    let response: TelegramResponse<Vec<TelegramUpdate>> = client
        .get(url)
        .query(&query)
        .send()
        .context("failed to poll Telegram updates")?
        .error_for_status()
        .context("Telegram getUpdates returned an error status")?
        .json()
        .context("failed to decode Telegram updates")?;

    telegram_result(response)
}

fn send_message(client: &Client, token: &str, chat_id: i64, text: &str) -> Result<()> {
    let url = telegram_url(token, "sendMessage");
    let form = vec![
        ("chat_id".to_string(), chat_id.to_string()),
        ("text".to_string(), text.to_string()),
    ];

    let response: TelegramResponse<serde_json::Value> = client
        .post(url)
        .form(&form)
        .send()
        .context("failed to send Telegram message")?
        .error_for_status()
        .context("Telegram sendMessage returned an error status")?
        .json()
        .context("failed to decode Telegram sendMessage response")?;

    telegram_result(response).map(|_| ())
}

fn send_document(
    client: &Client,
    token: &str,
    chat_id: i64,
    path: &Path,
    caption: &str,
) -> Result<()> {
    let url = telegram_url(token, "sendDocument");
    let document = Part::file(path)
        .with_context(|| format!("failed to attach {}", path.display()))?
        .mime_str("application/epub+zip")
        .context("failed to set EPUB MIME type")?;
    let form = Form::new()
        .text("chat_id", chat_id.to_string())
        .text("caption", caption.to_string())
        .part("document", document);

    let response: TelegramResponse<serde_json::Value> = client
        .post(url)
        .multipart(form)
        .send()
        .with_context(|| format!("failed to send {}", path.display()))?
        .error_for_status()
        .context("Telegram sendDocument returned an error status")?
        .json()
        .context("failed to decode Telegram sendDocument response")?;

    telegram_result(response).map(|_| ())
}

fn telegram_url(token: &str, method: &str) -> String {
    format!("https://api.telegram.org/bot{token}/{method}")
}

fn telegram_result<T>(response: TelegramResponse<T>) -> Result<T> {
    if response.ok {
        Ok(response.result)
    } else {
        bail!(
            "Telegram API error: {}",
            response
                .description
                .unwrap_or_else(|| "unknown".to_string())
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Chapter {
    novel_name: String,
    chapter_name: String,
    text: String,
}

fn parse_chapter(html: &str) -> Result<Chapter> {
    let document = Html::parse_document(html);
    let novel_name = select_one_text(&document, "#novel-name")
        .context("no novel name found with selector #novel-name")?;
    let chapter_name = select_one_text(&document, "#chapter-name")
        .context("no chapter name found with selector #chapter-name")?;
    let text = extract_chapter_text(&document)?;

    Ok(Chapter {
        novel_name,
        chapter_name,
        text,
    })
}

fn select_one_text(document: &Html, selector: &str) -> Result<String> {
    let selector = Selector::parse(selector).map_err(|_| anyhow::anyhow!("invalid selector"))?;
    let text = document
        .select(&selector)
        .next()
        .map(|node| normalize_spaces(&node.text().collect::<Vec<_>>().join(" ")))
        .filter(|text| !text.is_empty());

    text.with_context(|| format!("missing text for selector {selector:?}"))
}

fn extract_chapter_text(document: &Html) -> Result<String> {
    let selector = Selector::parse("#chapter p").expect("valid CSS selector");

    let mut seen = HashSet::new();
    let mut paragraphs = Vec::new();

    for paragraph in document.select(&selector) {
        let text = paragraph.text().collect::<Vec<_>>().join(" ");
        let text = normalize_spaces(&text);

        if text.is_empty() {
            continue;
        }

        let fingerprint = text.to_lowercase();
        if seen.insert(fingerprint) {
            paragraphs.push(text);
        }
    }

    if paragraphs.is_empty() {
        bail!("no chapter paragraphs found with selector #chapter p");
    }

    Ok(paragraphs.join("\n\n"))
}

fn normalize_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn sanitize_path_component(value: &str, fallback: &str) -> String {
    let mut output = String::with_capacity(value.len());

    for character in value.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, ' ' | '-' | '_' | '.') {
            output.push(character);
        }
    }

    let output = normalize_spaces(&output).replace(' ', "_");
    if output.is_empty() {
        fallback.to_string()
    } else {
        output
    }
}

fn novel_dir(output_root: &Path, novel_id: &str, novel_name: &str) -> std::path::PathBuf {
    output_root.join(format!(
        "{}_{}",
        sanitize_path_component(novel_id, "novel"),
        sanitize_path_component(novel_name, "novel")
    ))
}

fn find_existing_novel_dir(
    output_root: &Path,
    novel_id: &str,
) -> Result<Option<std::path::PathBuf>> {
    if !output_root.exists() {
        return Ok(None);
    }

    let prefix = format!("{}_", sanitize_path_component(novel_id, "novel"));
    let mut matches = Vec::new();

    for entry in fs::read_dir(output_root)
        .with_context(|| format!("failed to read {}", output_root.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&prefix) {
            matches.push(entry.path());
        }
    }

    if matches.len() > 1 {
        bail!(
            "multiple output folders found for novel id {novel_id}; keep only one: {:?}",
            matches
        );
    }

    Ok(matches.pop())
}

fn highest_existing_chapter(dir: &Path) -> Result<u32> {
    let mut highest = 0;

    for chapter in chapter_files(dir)? {
        highest = highest.max(chapter.number);
    }

    Ok(highest)
}

#[derive(Debug)]
struct ChapterFile {
    number: u32,
    title: String,
    path: std::path::PathBuf,
}

fn chapter_files(dir: &Path) -> Result<Vec<ChapterFile>> {
    let mut chapters = Vec::new();

    if !dir.exists() {
        return Ok(chapters);
    }

    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }

        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("txt") {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        let Some((number, title)) = parse_chapter_file_name(file_name) else {
            continue;
        };

        chapters.push(ChapterFile {
            number,
            title,
            path,
        });
    }

    chapters.sort_by_key(|chapter| chapter.number);
    Ok(chapters)
}

fn parse_chapter_file_name(file_name: &str) -> Option<(u32, String)> {
    let (number, rest) = file_name.split_once(" - ")?;
    let number = number.parse().ok()?;
    let title = rest.strip_suffix(".txt")?.replace('_', " ");
    Some((number, title))
}

fn build_epub(dir: &Path, novel_id: &str) -> Result<std::path::PathBuf> {
    let chapters = chapter_files(dir)?;
    if chapters.is_empty() {
        bail!("no chapter text files found in {}", dir.display());
    }

    let novel_title = epub_title_from_dir(dir, novel_id);
    let epub_path = dir.join("novel.epub");
    let file = File::create(&epub_path)
        .with_context(|| format!("failed to create {}", epub_path.display()))?;
    let mut zip = ZipWriter::new(file);
    let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    let deflated = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

    zip.start_file("mimetype", stored)?;
    zip.write_all(b"application/epub+zip")?;

    zip.start_file("META-INF/container.xml", deflated)?;
    zip.write_all(container_xml().as_bytes())?;

    let book_id = epub_book_id(novel_id);

    zip.start_file("OEBPS/content.opf", deflated)?;
    zip.write_all(content_opf(&novel_title, &book_id, &chapters).as_bytes())?;

    zip.start_file("OEBPS/toc.ncx", deflated)?;
    zip.write_all(toc_ncx(&novel_title, &book_id, &chapters).as_bytes())?;

    zip.start_file("OEBPS/style.css", deflated)?;
    zip.write_all(
        b"body{font-family:serif;line-height:1.55;} p{margin:0 0 1em;} h1{font-size:1.4em;}",
    )?;

    for chapter in &chapters {
        let text = fs::read_to_string(&chapter.path)
            .with_context(|| format!("failed to read {}", chapter.path.display()))?;
        zip.start_file(format!("OEBPS/{}", chapter_href(chapter)), deflated)?;
        zip.write_all(chapter_xhtml(chapter, &text).as_bytes())?;
    }

    zip.finish()?;
    Ok(epub_path)
}

fn epub_title_from_dir(dir: &Path, novel_id: &str) -> String {
    let name = dir
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("novel");
    let prefix = format!("{}_", sanitize_path_component(novel_id, "novel"));
    name.strip_prefix(&prefix).unwrap_or(name).replace('_', " ")
}

fn epub_book_id(novel_id: &str) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("mvlempyr-{novel_id}-{timestamp}")
}

fn container_xml() -> String {
    r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="OEBPS/content.opf" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>
"#
    .to_string()
}

fn content_opf(title: &str, book_id: &str, chapters: &[ChapterFile]) -> String {
    let mut manifest = String::new();
    let mut spine = String::new();

    for chapter in chapters {
        manifest.push_str(&format!(
            r#"    <item id="chapter-{number:04}" href="{href}" media-type="application/xhtml+xml"/>
"#,
            number = chapter.number,
            href = chapter_href(chapter)
        ));
        spine.push_str(&format!(
            r#"    <itemref idref="chapter-{number:04}"/>
"#,
            number = chapter.number
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<package xmlns="http://www.idpf.org/2007/opf" unique-identifier="book-id" version="2.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:identifier id="book-id">{book_id}</dc:identifier>
    <dc:title>{title}</dc:title>
    <dc:creator>Unknown</dc:creator>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
    <item id="style" href="style.css" media-type="text/css"/>
{manifest}  </manifest>
  <spine toc="ncx">
{spine}  </spine>
</package>
"#,
        book_id = escape_xml(book_id),
        title = escape_xml(title),
        manifest = manifest,
        spine = spine
    )
}

fn toc_ncx(title: &str, book_id: &str, chapters: &[ChapterFile]) -> String {
    let mut nav_points = String::new();

    for (index, chapter) in chapters.iter().enumerate() {
        nav_points.push_str(&format!(
            r#"    <navPoint id="navPoint-{number:04}" playOrder="{order}">
      <navLabel><text>{title}</text></navLabel>
      <content src="{href}"/>
    </navPoint>
"#,
            number = chapter.number,
            order = index + 1,
            title = escape_xml(&chapter.title),
            href = chapter_href(chapter)
        ));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head>
    <meta name="dtb:uid" content="{book_id}"/>
    <meta name="dtb:depth" content="1"/>
    <meta name="dtb:totalPageCount" content="0"/>
    <meta name="dtb:maxPageNumber" content="0"/>
  </head>
  <docTitle><text>{title}</text></docTitle>
  <navMap>
{nav_points}  </navMap>
</ncx>
"#,
        book_id = escape_xml(book_id),
        title = escape_xml(title),
        nav_points = nav_points
    )
}

fn chapter_xhtml(chapter: &ChapterFile, text: &str) -> String {
    let mut paragraphs = String::new();

    for paragraph in text.split("\n\n") {
        let paragraph = normalize_spaces(paragraph);
        if paragraph.is_empty() {
            continue;
        }
        paragraphs.push_str(&format!("  <p>{}</p>\n", escape_xml(&paragraph)));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN"
  "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml" xml:lang="en">
<head><title>{title}</title><link rel="stylesheet" type="text/css" href="../style.css"/></head>
<body>
  <h1>{title}</h1>
{paragraphs}</body>
</html>
"#,
        title = escape_xml(&chapter.title),
        paragraphs = paragraphs
    )
}

fn chapter_href(chapter: &ChapterFile) -> String {
    format!("chapters/chapter_{:04}.xhtml", chapter.number)
}

fn escape_xml(value: &str) -> String {
    let mut output = String::with_capacity(value.len());

    for character in value.chars() {
        match character {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#39;"),
            _ => output.push(character),
        }
    }

    output
}

fn write_output(path: &Path, chapter: &str) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create output directory {}", parent.display()))?;
    }

    fs::write(path, chapter).with_context(|| format!("failed to write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_only_chapter_paragraphs_and_dedupes() {
        let html = r#"
            <nav><p>menu</p></nav>
            <h1 id="novel-name">Test Novel</h1>
            <h2 id="chapter-name">Chapter 1: Start</h2>
            <div id="chapter">
                <p> First   paragraph. </p>
                <p>Second paragraph.</p>
                <p>first paragraph.</p>
            </div>
            <footer><p>footer</p></footer>
        "#;

        let chapter = parse_chapter(html).unwrap();
        assert_eq!(chapter.novel_name, "Test Novel");
        assert_eq!(chapter.chapter_name, "Chapter 1: Start");
        assert_eq!(chapter.text, "First paragraph.\n\nSecond paragraph.");
    }

    #[test]
    fn sanitizes_file_names() {
        assert_eq!(
            sanitize_path_component("Chapter 1: You're Not Dead, Are You?", "chapter"),
            "Chapter_1_Youre_Not_Dead_Are_You"
        );
    }

    #[test]
    fn builds_epub_from_existing_chapter_files() {
        let dir = std::env::temp_dir().join(format!(
            "chapter_extract_test_{}_{}",
            std::process::id(),
            "epub"
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("0001 - Chapter_1_Start.txt"), "One.\n\nTwo.").unwrap();
        fs::write(dir.join("0002 - Chapter_2_Next.txt"), "Three.").unwrap();

        let epub = build_epub(&dir, "13736").unwrap();
        assert!(epub.exists());
        assert!(fs::metadata(&epub).unwrap().len() > 0);

        let file = File::open(epub).unwrap();
        let mut archive = zip::ZipArchive::new(file).unwrap();
        assert!(archive.by_name("mimetype").is_ok());
        assert!(archive.by_name("OEBPS/content.opf").is_ok());
        assert!(archive.by_name("OEBPS/chapters/chapter_0001.xhtml").is_ok());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_telegram_novel_command() {
        assert_eq!(
            parse_novel_command("/novel 13736"),
            Some("13736".to_string())
        );
        assert_eq!(
            parse_novel_command("/novel@TestBot 13736"),
            Some("13736".to_string())
        );
        assert_eq!(parse_novel_command("/novel abc"), None);
        assert_eq!(parse_novel_command("13736"), None);
    }
}
