use rinja::Template;
use serde::Deserialize;
use std::sync::Arc;
use tiny_http::{Header, Response, Server, StatusCode};

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
    let parser =
        Parser::new_ext(&contents, options).filter_map(|event| match event {
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
        meta: meta.unwrap_or_default(),
        markdown: &html_output,
    };
    template.render().unwrap()
}

fn serve(server: Arc<Server>) {
    let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    loop {
        let rq = server.recv().unwrap();
        let url = &rq.url()[1..];
        let path = std::path::absolute(cwd.join(url)).unwrap();
        if !path.starts_with(cwd.as_path()) {
            eprintln!(
                "Path (\"{}\") is outside working directory (\"{}\")",
                path.display(),
                cwd.display()
            );
            rq.respond(Response::new_empty(StatusCode(404))).unwrap();
            continue;
        }
        if !path.is_file() {
            rq.respond(Response::new_empty(StatusCode(404))).unwrap();
            continue;
        }

        let contents = std::fs::read(&path).unwrap();
        match path.extension().and_then(|x| x.to_str()) {
            Some("md") => {
                let contents = String::from_utf8(contents).unwrap();
                let contents = markdown_to_document(&contents);
                rq.respond(Response::from_string(contents).with_header(
                    Header::from_bytes(b"Content-Type", b"text/html").unwrap(),
                ))
                .unwrap();
            }
            None | Some(_) => {
                rq.respond(Response::from_data(contents)).unwrap();
            }
        }
    }
}

fn main() {
    let server_tasks = 4;
    let server = Server::http("127.0.0.1:6969").unwrap();
    let server = Arc::new(server);
    let mut guards = Vec::with_capacity(server_tasks);
    for _ in 0..server_tasks {
        let server = server.clone();
        let guard = std::thread::spawn(move || serve(server));

        guards.push(guard);
    }

    for guard in guards {
        guard.join().unwrap();
    }
}
