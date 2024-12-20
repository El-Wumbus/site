use chrono::NaiveDate;
use clap::Parser;
use eyre::eyre;
use log::{debug, error, info};
use rinja::Template;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tiny_http::{Header, Request, Response, Server, StatusCode};

const STYLES: &str = include_str!("styles.css");

#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Where to serve content from (the current working directory is used if
    /// omitted).
    content_path: Option<PathBuf>,
    /// Which socket address and port to use
    #[arg(long, default_value = "127.0.0.1:0")]
    bind: std::net::SocketAddr,
    /// How much to parallelize serving web pages
    #[arg(short = 't', long, default_value_t = 4)]
    serve_threads: usize,
}

type DocIndex = Vec<(Meta, String)>;

fn main() -> eyre::Result<()> {
    env_logger::Builder::from_default_env()
        .filter(None, log::LevelFilter::Debug)
        .init();

    let args = Args::parse();
    let content_path = args.content_path.unwrap_or_else(|| {
        std::env::current_dir().expect("failed to get current directory")
    });
    let content_path = std::fs::canonicalize(content_path)?;

    let server = Server::http(args.bind).map_err(|e| eyre!("{e}"))?;
    info!("Spawned server on address: {}", server.server_addr());

    let mut document_index = {
        let mut document_index: DocIndex = vec![];
        walk(&content_path, &mut |is_dir, path| {
            if path
                .file_name()
                .is_some_and(|x| x.as_encoded_bytes().starts_with(b"."))
            {
                return Ok(false);
            }
            if !is_dir
                && path
                    .extension()
                    .is_some_and(|x| x == "md" || x == "markdown")
            {
                debug_assert!(path.is_absolute());
                let contents = std::fs::read_to_string(path)?;
                if let (_, Some(meta)) = markdown_to_document(&contents) {
                    let path = path
                        .strip_prefix(&content_path)
                        .expect("is a subdir of content path");
                    document_index.push((meta, path.to_str().unwrap().to_string()));
                }
            }
            Ok(true)
        })?;
        Arc::new(RwLock::new(document_index))
    };
    let server = Arc::new(server);
    let content_path: Arc<Path> = content_path.as_path().into();
    let mut guards = Vec::with_capacity(args.serve_threads);
    for _ in 0..args.serve_threads {
        let server = server.clone();
        let content_path = content_path.clone();
        let document_index = document_index.clone();
        let guard =
            std::thread::spawn(move || serve(server, document_index, content_path));
        guards.push(guard);
    }

    for guard in guards {
        guard.join().map_err(|e| eyre!("{e:?}"))??;
    }
    Ok(())
}

fn walk(
    p: impl AsRef<std::path::Path>,
    callback: &mut dyn FnMut(bool, &std::path::Path) -> std::io::Result<bool>,
) -> Result<(), std::io::Error> {
    let dir = p.as_ref();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                if callback(true, &path)? {
                    walk(path, callback)?;
                }
            } else {
                callback(false, &path)?;
            }
        }
    } else {
        // We don't want to ignore the first item if it's a file
        callback(false, dir)?;
    }
    Ok(())
}

/// ```rinja
/// <!doctype html>
/// <html>
/// <head>
/// <meta charset="utf-8">
/// <style>{{ styles }}</style></head>
/// <body>
/// <ol>
/// {% for doc in docs %}
///     <li><a href="/{{doc.1}}"> {{ doc.0.date }} —  {{doc.0.title}} </a></li>
/// {% endfor %}
/// </ol>
/// </body>
/// </html>
/// ```
#[derive(Template)]
#[template(ext = "html", escape = "none", in_doc = true)]
struct IndexTemplate<'a> {
    styles: &'static str,
    docs: &'a [(&'a Meta, &'a str)],
}
impl IndexTemplate<'_> {
    fn index(docs: Arc<RwLock<DocIndex>>) -> String {
        let docs = docs.read().unwrap();
        let ds: Vec<(&Meta, &str)> =
            docs.iter().map(|(meta, s)| (meta, s.as_str())).collect();
        let template = IndexTemplate {
            styles: STYLES,
            docs: ds.as_slice(),
        };

        template.render().unwrap()
    }
}

fn serve(
    server: Arc<Server>,
    index: Arc<RwLock<DocIndex>>,
    content_dir: Arc<Path>,
) -> eyre::Result<()> {
    let html_header = Header::from_bytes(b"Content-Type", b"text/html").unwrap();
    loop {
        let rq = server.recv().unwrap();
        let url = &rq.url()[1..];

        if url == "index" || url == "index.html" {
            respond(
                rq,
                Response::from_string(IndexTemplate::index(index.clone()))
                    .with_header(html_header.clone()),
            );
            continue;
        }

        let path = match std::path::absolute(content_dir.join(url)) {
            Err(e) => {
                error!("Failed to make request url (\"{url}\") absolute: {e}");
                respond(rq, Response::new_empty(StatusCode(400)));
                continue;
            }
            Ok(p) => p,
        };

        if !path.starts_with(&content_dir)
            || path
                .file_name()
                .is_some_and(|x| x.as_encoded_bytes().starts_with(b"."))
            || !path.is_file()
        {
            respond(rq, Response::new_empty(StatusCode(404)));
            continue;
        }

        info!("Responding to request for \"{}\"", path.display());
        let contents = match std::fs::read(&path) {
            Ok(c) => c,
            Err(e) => {
                error!("Error getting \"{}\": {e}", path.display());
                continue;
            }
        };
        match path.extension().and_then(|x| x.to_str()) {
            Some("md") | Some("markdown") => {
                let contents = String::from_utf8(contents).unwrap();
                let (contents, _) = markdown_to_document(&contents);
                if respond(
                    rq,
                    Response::from_string(contents).with_header(html_header.clone()),
                ) {
                    continue;
                }
            }
            None | Some(_) => {
                if respond(rq, Response::from_data(contents)) {
                    continue;
                }
            }
        }
    }
}

#[derive(Template)]
#[template(ext = "html", escape = "none", path = "document.html")]
struct DocumentTemplate<'a> {
    styles: &'static str,
    meta: Meta,
    markdown: &'a str,
}

#[derive(Debug, Clone, Deserialize)]
struct Meta {
    title: String,
    date: NaiveDate,
    lang: Option<String>,
    desc: Option<String>,
}

impl Default for Meta {
    fn default() -> Self {
        Self {
            title: "UNTITLED!".to_string(),
            date: NaiveDate::from_ymd_opt(2024, 01, 01).unwrap(),
            lang: None,
            desc: None,
        }
    }
}

fn markdown_to_document(contents: &str) -> (String, Option<Meta>) {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    use std::sync::LazyLock;
    use syntect::highlighting::{Theme, ThemeSet};
    use syntect::parsing::SyntaxSet;
    static SYNTAX_SET: LazyLock<SyntaxSet> =
        LazyLock::new(SyntaxSet::load_defaults_newlines);
    static THEME: LazyLock<Theme> = LazyLock::new(|| {
        let theme_set = ThemeSet::load_defaults();
        theme_set.themes["base16-ocean.dark"].clone()
    });

    let mut options = Options::empty();
    options.insert(Options::ENABLE_GFM);

    #[derive(Default)]
    enum State {
        #[default]
        Normal,
        Meta,
        Highlight,
    }
    let mut state = State::default();
    let mut code = String::new();
    let mut meta = None;
    let mut syntax = SYNTAX_SET.find_syntax_plain_text();
    let parser = Parser::new_ext(&contents, options).filter_map(|event| match event {
        Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
            let lang = lang.trim();
            if lang == "meta" {
                state = State::Meta;
                None
            } else {
                state = State::Highlight;
                syntax = SYNTAX_SET
                    .find_syntax_by_token(&lang)
                    .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
                None
            }
        }
        Event::Text(text) => match state {
            State::Normal => Some(Event::Text(text)),
            State::Meta => {
                match toml::de::from_str::<Meta>(&text) {
                    Ok(m) => meta = Some(m),
                    Err(e) => {
                        error!("Failed to parse invalid metadata: {e}")
                    }
                }
                None
            }
            State::Highlight => {
                code.push_str(&text);
                None
            }
        },
        Event::End(TagEnd::CodeBlock) => match state {
            State::Normal => Some(Event::End(TagEnd::CodeBlock)),
            State::Meta => {
                state = State::Normal;
                None
            }
            State::Highlight => {
                let html = syntect::html::highlighted_html_for_string(
                    &code,
                    &SYNTAX_SET,
                    syntax,
                    &THEME,
                )
                .unwrap_or(code.clone());
                code.clear();
                state = State::Normal;
                Some(Event::Html(html.into()))
            }
        },
        _ => Some(event),
    });

    let mut html_output = String::new();
    pulldown_cmark::html::push_html(&mut html_output, parser);
    let template = DocumentTemplate {
        styles: STYLES,
        meta: meta.clone().unwrap_or_default(),
        markdown: &html_output,
    };
    let html = template.render().unwrap();
    (html, meta)
}

fn respond<R: std::io::Read>(request: Request, response: Response<R>) -> bool {
    let url = request.url().to_string();
    if let Err(e) = request.respond(response) {
        error!("Failed to respond to request for \"{url}\": {e}");
        return true;
    }
    false
}
