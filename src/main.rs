use eyre::eyre;
use log::{debug, error, info, warn};
use rinja::Template;
use serde::Deserialize;
use std::sync::Arc;
use std::path::{Path, PathBuf};
use tiny_http::{Header, Request, Response, Server, StatusCode};

#[derive(Template)]
#[template(ext = "html", escape = "none", path = "document.html")]
struct DocumentTemplate<'a> {
    meta: Meta,
    markdown: &'a str,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Meta {
    title: Option<String>,
    lang: Option<String>,
    desc: Option<String>,
}

fn markdown_to_document(contents: &str) -> String {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
    use std::sync::LazyLock;
    use syntect::highlighting::{Theme, ThemeSet};
    use syntect::parsing::SyntaxSet;
    static SYNTAX_SET: LazyLock<SyntaxSet> = LazyLock::new(SyntaxSet::load_defaults_newlines);
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
                meta = Some(toml::de::from_str::<Meta>(&text).unwrap());
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
                let html =
                    syntect::html::highlighted_html_for_string(&code, &SYNTAX_SET, syntax, &THEME)
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
        meta: meta.unwrap_or_default(),
        markdown: &html_output,
    };
    template.render().unwrap()
}

fn respond<R: std::io::Read>(request: Request, response: Response<R>) -> bool {
    let url = request.url().to_string();
    if let Err(e) = request.respond(response) {
        error!("Failed to respond to request for \"{url}\": {e}");
        return true;
    }
    false
}

fn serve(server: Arc<Server>, content_dir: Arc<Path>) -> eyre::Result<()> {
    loop {
        let rq = server.recv().unwrap();
        let url = &rq.url()[1..];
        let path = std::path::absolute(content_dir.join(url)).unwrap();
        if !path.starts_with(&content_dir) {
            debug!(
                "Requested path (\"{}\") is outside content directory (\"{}\")",
                path.display(),
                content_dir.display()
            );
            respond(rq, Response::new_empty(StatusCode(404)));
            continue;
        }
        if !path.is_file() {
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
            Some("md") => {
                let contents = String::from_utf8(contents).unwrap();
                let contents = markdown_to_document(&contents);
                if respond(
                    rq,
                    Response::from_string(contents)
                        .with_header(Header::from_bytes(b"Content-Type", b"text/html").unwrap()),
                ) {
                    continue;
                }
            }
            None | Some(_) => {
                if respond(rq, Response::from_data(contents)) {
                    continue;
                };
            }
        }
    }
}

fn main() -> eyre::Result<()> {
    let mut builder = env_logger::Builder::from_default_env()
        .filter(None, log::LevelFilter::Info)
        .init();
    let mut args = std::env::args_os().skip(1);
    let content_path = args
        .next()
        .map(|x| std::path::PathBuf::from(x))
        .unwrap_or_else(|| std::env::current_dir().expect("failed to get current directory"));
    let content_path = std::fs::canonicalize(content_path)?;
    let server_tasks = 4;
    let server = Server::http("127.0.0.1:6969").map_err(|e| eyre!("{e}"))?;
    info!("Spawned server on address: {}", server.server_addr());

    let server = Arc::new(server);
    let content_path: Arc<Path> = content_path.as_path().into();
    let mut guards = Vec::with_capacity(server_tasks);
    for _ in 0..server_tasks {
        let server = server.clone();
        let content_path = content_path.clone();
        let guard = std::thread::spawn({
            move || serve(server, content_path)});
        guards.push(guard);
    }

    for guard in guards {
        guard.join().map_err(|e| eyre!("{e:?}"))??;
    }
    Ok(())
}
