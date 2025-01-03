#![feature(str_split_remainder)]

use chrono::NaiveDate;
use clap::Parser;
use eyre::eyre;
use include_dir::include_dir;
use log::{debug, error, info};
use rinja::Template;
use serde::Deserialize;
use signal_hook::consts::signal::SIGHUP;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tiny_http::{Header, Request, Response, Server, StatusCode};
use url::Url;

static ASSETS: include_dir::Dir<'_> =
    include_dir!("$CARGO_MANIFEST_DIR/static-assets");

static STYLES: include_dir::Dir<'_> =
    include_dir!("$CARGO_MANIFEST_DIR/styles");
// const STYLES: &str = include_str!("../styles/styles.css");

#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Where to serve content from (the current working directory is used if
    /// omitted).
    content_path: Option<PathBuf>,
    /// Which socket address and port to use
    #[arg(long, default_value = "127.0.0.2:6969")]
    bind: std::net::SocketAddr,
    #[arg(short = 't', long, default_value_t = 4)]
    serve_threads: usize,
}

fn main() -> eyre::Result<()> {
    let args = Args::parse();
    env_logger::Builder::from_default_env()
        .filter(None, log::LevelFilter::Trace)
        .init();

    let reload_state = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(SIGHUP, reload_state.clone())?;

    let content_path: Arc<Path> =
        std::fs::canonicalize(args.content_path.unwrap_or_else(|| {
            std::env::current_dir().expect("current directory")
        }))?
        .as_path()
        .into();

    let state = Arc::new(RwLock::new(State::load(&content_path)?));
    let server = Arc::new(Server::http(args.bind).map_err(|e| eyre!("{e}"))?);
    info!("Spawned server on address: http://{}", server.server_addr());

    for _ in 0..args.serve_threads {
        let server = server.clone();
        let content_path = content_path.clone();
        let state = state.clone();

        std::thread::spawn(move || serve(server, state, content_path));
    }

    loop {
        if reload_state.swap(false, Ordering::Relaxed) {
            info!("Reloading state...");
            let mut state = state.write().unwrap();
            match State::load(&content_path) {
                Ok(s) => {
                    info!("State reloaded sucessfully!");
                    *state = s;
                }
                Err(e) => error!(
                    "Failed to reload state (retaining previous state): {e}"
                ),
            }
        }

        std::thread::sleep(std::time::Duration::from_millis(256));
    }
}

#[derive(Debug)]
struct IndexEntry {
    meta: Meta,
    section: String,
    path: String,
}

#[derive(Debug)]
struct State {
    sections: Vec<String>,
    index: Vec<IndexEntry>,
}

impl State {
    fn load(content_path: &Path) -> eyre::Result<State> {
        let found_git = find_program("git").is_some();

        let mut index = vec![];
        let mut sections = vec![];

        walk(content_path, &mut |is_dir, path| {
            if let Some(file_name) = path.file_name() {
                if file_name == ".section.toml" && !is_dir {
                    // TODO: REWORK sections.
                    /*let section_cfg = std::fs::read_to_string(path)?;
                    let section_cfg = match toml::de::from_str::<Section>(&section_cfg) {
                        Ok(c) => c,
                        Err(e) => {
                            error!("Failed to parse section configuration at path \"{}\": {}");
                            Section::default()
                        }
                    };*/
                    let path = path
                            .strip_prefix(content_path)
                            .expect("is a subdir of content path");
                    if let Some(section_name) = path
                        .components()
                        .next()
                        .map(|x| x.as_os_str())
                        .map(|x| x.to_str().unwrap().to_string())
                    {
                        sections.push(section_name);
                    }
                }

                if file_name.as_encoded_bytes().starts_with(b".") {
                    return Ok(false);
                }
            }

            if is_dir {
                return Ok(true);
            }

            match path.extension().and_then(|x| x.to_str()) {
                Some("md" | "markdown") => {
                    debug_assert!(path.is_absolute());
                    let contents = std::fs::read_to_string(path)?;
                    if let (_, Some(meta)) =
                        markdown_to_document(&sections, &contents)
                    {
                        let path = path
                            .strip_prefix(content_path)
                            .expect("is a subdir of content path");
                        let section = path
                            .components()
                            .next()
                            .map(|x| x.as_os_str())
                            .map(|x| x.to_str().unwrap().to_string())
                            .unwrap_or_default();
                        let path = path.to_str().unwrap().to_string();
                        let section = if section == path {
                            String::new()
                        } else {
                            section
                        };

                        index.push(IndexEntry {
                            meta,
                            section,
                            path,
                        });
                    }
                }
                _ => {}
            }
            
            Ok(true)
        })?;

        sections.retain(|s| index.iter().any(|i| i.section == *s));
        if found_git {
            if !sections.is_empty() {
                let ignored =
                    filter_ignored(content_path, sections.as_slice())?;
                debug!("Removing ignored sections: {ignored:?}");
                sections
                    .retain(|s| !ignored.iter().any(|x| *x == Path::new(s)));
            }

            if !index.is_empty() {
                let ignored = filter_ignored(
                    content_path,
                    &index.iter().map(|x| x.path.as_str()).collect::<Vec<_>>(),
                )?;
                debug!(
                    "Removing ignored documents from the index: {ignored:?}"
                );
                index.retain(|i| {
                    !ignored.iter().any(|x| *x == Path::new(&i.path))
                });
            }
        }

        sections.push(String::new()); // Blank is the root index
        sections.sort();
        index.sort_by(|r, l| l.meta.date.cmp(&r.meta.date));
        Ok(State { sections, index })
    }
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

#[derive(Template)]
#[template(ext = "html", path = "header.html")]
struct HeaderTemplate<'a> {
    sects: &'a [&'a str],
}

#[derive(Template)]
#[template(ext = "html", escape = "none", path = "index.html")]
struct IndexTemplate<'a> {
    header: HeaderTemplate<'a>,
    styles: &'static str,
    docs: &'a [IndexTemplateEntryData<'a>],
}
struct IndexTemplateEntryData<'a> {
    meta: &'a Meta,
    section: &'a str,
    path: &'a str,
}

impl<'a> From<&'a IndexEntry> for IndexTemplateEntryData<'a> {
    fn from(ie: &'a IndexEntry) -> Self {
        Self {
            meta: &ie.meta,
            section: ie.section.as_str(),
            path: ie.path.as_str(),
        }
    }
}

impl IndexTemplate<'_> {
    fn index(
        sections: &[String],
        docs: &[IndexEntry],
        section: Option<&str>,
    ) -> String {
        let docs: Vec<IndexTemplateEntryData> = if let Some(section) = section {
            docs.iter()
                .filter(|x| x.path.starts_with(section))
                .map(|x| x.into())
                .collect()
        } else {
            docs.iter().map(|x| x.into()).collect()
        };
        let sections = sections.iter().map(String::as_str).collect::<Vec<_>>();
        let template = IndexTemplate {
            header: HeaderTemplate {
                sects: sections.as_slice(),
            },
            styles: STYLES
                .get_file("styles.css")
                .and_then(include_dir::File::contents_utf8)
                .unwrap(),
            docs: docs.as_slice(),
        };

        template.render().unwrap()
    }
}

fn serve(
    server: Arc<Server>,
    state: Arc<RwLock<State>>,
    content_dir: Arc<Path>,
) -> eyre::Result<()> {
    let html_header =
        Header::from_bytes(b"Content-Type", b"text/html").unwrap();
    loop {
        let rq = server.recv().unwrap();
        let headers = rq.headers();
        // Why is tiny_http using this `AsciiStr` haufen scheiße?
        let Some(host) = headers
            .iter()
            .find(|x| x.field.as_str().as_str().eq_ignore_ascii_case("Host"))
        else {
            // The host header is required: https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Host
            respond(rq, Response::new_empty(StatusCode(400)));
            continue;
        };
        // Tiny URL gives me a fake URL, so I have to first construct a URL,
        // then deconstruct it.
        let url = format!("http://{}{}", host.value, rq.url());
        let url = match Url::parse(&url) {
            Ok(url) => url,
            Err(e) => {
                error!("Invalid URL \"{url}\": {e}");
                continue;
            }
        };

        let path = url.path();
        match path {
            "/" => {
                respond(
                    rq,
                    Response::new_empty(StatusCode(308)).with_header(
                        Header::from_bytes(b"location", b"/index.html")
                            .unwrap(),
                    ),
                );
                continue;
            }
            "/index.html" => {
                let state_l = state.read().unwrap();
                respond(
                    rq,
                    Response::from_string(IndexTemplate::index(
                        state_l.sections.as_slice(),
                        state_l.index.as_slice(),
                        None,
                    ))
                    .with_header(html_header.clone()),
                );
                continue;
            }
            _ if path.ends_with("/index.html") => {
                let section = &path.strip_suffix("/index.html").unwrap()[1..];
                let state_l = state.read().unwrap();
                respond(
                    rq,
                    Response::from_string(IndexTemplate::index(
                        state_l.sections.as_slice(),
                        state_l.index.as_slice(),
                        Some(section),
                    ))
                    .with_header(html_header.clone()),
                );
                continue;
            }
            _ if path.starts_with("/.static-assets") => {
                let mut segments = url.path_segments().unwrap();
                let _ = segments.next(); // I can't use Skip::remainder if I use iter::skip ????
                let Some(remainder) = segments.remainder() else {
                    respond(rq, Response::new_empty(StatusCode(404)));
                    continue;
                };
                if let Some(a) = ASSETS.get_file(remainder) {
                    respond(rq, Response::from_data(a.contents()));
                } else {
                    respond(rq, Response::new_empty(StatusCode(404)));
                };
                continue;
            }

            _ if path.starts_with("/.styles") => {
                let mut segments = url.path_segments().unwrap();
                let _ = segments.next(); // I can't use Skip::remainder if I use iter::skip ????
                let Some(remainder) = segments.remainder() else {
                    respond(rq, Response::new_empty(StatusCode(404)));
                    continue;
                };
                if let Some(a) = STYLES.get_file(remainder) {
                    respond(rq, Response::from_data(a.contents()));
                } else {
                    respond(rq, Response::new_empty(StatusCode(404)));
                };
                continue;
            }
            _ => {}
        }

        let path = &path[1..];
        let state_l = state.read().unwrap();

        // Ensure we don't serve anything that hasn't been indexed, this way
        // ignore files are honored.
        if !state_l.index.iter().any(|x| x.path == path) {
            respond(rq, Response::new_empty(StatusCode(404)));
            continue;
        }

        let path = match std::path::absolute(content_dir.join(path)) {
            Err(_) => {
                respond(rq, Response::new_empty(StatusCode(404)));
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
            Some("md" | "markdown") => {
                let contents = String::from_utf8(contents).unwrap();
                let state = state.read().unwrap();
                let (contents, _) =
                    markdown_to_document(&state.sections, &contents);
                if respond(
                    rq,
                    Response::from_string(contents)
                        .with_header(html_header.clone()),
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
    header: HeaderTemplate<'a>,
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
            date: NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
            lang: None,
            desc: None,
        }
    }
}

fn markdown_to_document(
    header_sections: &[String],
    contents: &str,
) -> (String, Option<Meta>) {
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

    #[derive(Default)]
    enum ParseState {
        #[default]
        Normal,
        Meta,
        Highlight,
    }

    let mut options = Options::empty();
    options.insert(Options::ENABLE_GFM);

    let mut state = ParseState::default();
    let mut code = String::new();
    let mut meta = None;
    let mut syntax = SYNTAX_SET.find_syntax_plain_text();
    let parser =
        Parser::new_ext(contents, options).filter_map(|event| match event {
            Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))) => {
                let lang = lang.trim();
                if lang == "meta" {
                    state = ParseState::Meta;
                    None
                } else {
                    state = ParseState::Highlight;
                    syntax = SYNTAX_SET
                        .find_syntax_by_token(lang)
                        .unwrap_or_else(|| SYNTAX_SET.find_syntax_plain_text());
                    None
                }
            }
            Event::Text(text) => match state {
                ParseState::Normal => Some(Event::Text(text)),
                ParseState::Meta => {
                    match toml::de::from_str::<Meta>(&text) {
                        Ok(m) => meta = Some(m),
                        Err(e) => error!("Failed to parse metadata: {e}"),
                    }
                    None
                }
                ParseState::Highlight => {
                    code.push_str(&text);
                    None
                }
            },
            Event::End(TagEnd::CodeBlock) => match state {
                ParseState::Normal => Some(Event::End(TagEnd::CodeBlock)),
                ParseState::Meta => {
                    state = ParseState::Normal;
                    None
                }
                ParseState::Highlight => {
                    let html = syntect::html::highlighted_html_for_string(
                        &code,
                        &SYNTAX_SET,
                        syntax,
                        &THEME,
                    )
                    .unwrap_or(code.clone());
                    code.clear();
                    state = ParseState::Normal;
                    Some(Event::Html(html.into()))
                }
            },
            _ => Some(event),
        });

    let mut html_output = String::new();
    pulldown_cmark::html::push_html(&mut html_output, parser);

    let sections = header_sections
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let template = DocumentTemplate {
        header: HeaderTemplate {
            sects: sections.as_slice(),
        },
        styles: STYLES
            .get_file("styles.css")
            .and_then(include_dir::File::contents_utf8)
            .unwrap(),
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

fn find_program(path: impl AsRef<Path>) -> Option<PathBuf> {
    let sps = std::env::var_os("PATH")?;
    for p in std::env::split_paths(&sps) {
        let path = p.join(&path);
        if path.is_file() {
            // I just assume that the file in the path is executable because I
            // don't want to check for that here.
            return Some(path);
        }
    }
    None
}

fn filter_ignored(
    in_dir: &Path,
    paths: &[impl AsRef<Path>],
) -> eyre::Result<Vec<PathBuf>> {
    let paths = paths.iter().map(|x| x.as_ref()).collect::<Vec<_>>();
    let mut git = std::process::Command::new("git");
    let git = git
        .current_dir(in_dir)
        .args(["check-ignore", "--"])
        .args(paths.as_slice());
    log::trace!("Running \"git\" with args: {:?}", git.get_args());

    let output = git.output()?;
    let stdout = String::from_utf8(output.stdout)?;
    let code = output
        .status
        .code()
        .ok_or_else(|| eyre!("git didn't exit with a code"))?;
    if code == 128 {
        let stderr = String::from_utf8(output.stderr)?;
        return Err(eyre!(
            "'Git check-ignore' exited uncuccessfully with output:\nstdout:{stdout}\nstderr:\n{stderr}"
        ));
    }
    Ok(stdout
        .lines()
        .map(|line| PathBuf::from(line.trim()))
        .collect())
}
