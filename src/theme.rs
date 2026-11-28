//! White-label presentation: the page template rendered with the branding
//! settings, the generated theme stylesheet, and static theme files.
//!
//! A theme directory (`--theme-dir` / `RESELLER_THEME_DIR`) may contain:
//!
//! - `index.html`: replaces the built-in page template, with the same placeholders.
//! - `theme.css`: appended to `/assets/theme.css` after the branding CSS.
//! - Any image, font, or text file, served at `/theme/<path>`. Use these for
//!   files such as a `logo_url`.
use crate::{App, config::Config};
use axum::{
    body::Body,
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
};
use serde_json::json;
use std::path::{Component, Path};

const BUILTIN_TEMPLATE: &str = include_str!("../assets/index.html");
const BUILTIN_LOGO: &str = "/assets/favicon.svg";
/// Largest theme file served, in bytes.
const MAX_THEME_FILE_BYTES: u64 = 5 * 1024 * 1024;
/// Largest custom `index.html` template read, in bytes.
const MAX_TEMPLATE_BYTES: u64 = 512 * 1024;

/// Escapes text for HTML element content and quoted attributes.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// MIME type for an allowed theme file extension.
fn content_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "avif" => "image/avif",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "txt" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

/// Branding values exposed to the browser app.
pub fn public_json(config: &Config) -> serde_json::Value {
    let b = &config.branding;
    json!({
        "name": b.name,
        "tagline": b.tagline,
        "hero_eyebrow": b.hero_eyebrow,
        "hero_title": b.hero_title,
        "hero_text": b.hero_text,
        "legal_name": b.legal_name,
        "contact_email": b.contact_email,
        "terms_url": b.terms_url,
        "privacy_url": b.privacy_url,
        "color_scheme": b.color_scheme,
        "theme_toggle": b.theme_toggle,
        "request_estimate_usd": config.billing.request_estimate_usd,
    })
}

/// Renders the page template with the current branding.
pub fn render(template: &str, config: &Config) -> String {
    let b = &config.branding;
    let favicon = if b.favicon_url.is_empty() {
        "<link rel=\"icon\" type=\"image/svg+xml\" href=\"/assets/favicon.svg\">\n  \
         <link rel=\"icon\" type=\"image/png\" sizes=\"32x32\" href=\"/assets/favicon-32.png\">"
            .to_owned()
    } else {
        let mime = content_type(Path::new(
            b.favicon_url.split(['?', '#']).next().unwrap_or(""),
        ))
        .filter(|m| m.starts_with("image/"))
        .map(|m| format!(" type=\"{m}\""))
        .unwrap_or_default();
        format!(
            "<link rel=\"icon\"{mime} href=\"{}\">",
            escape(&b.favicon_url)
        )
    };
    let links = b
        .footer_links
        .iter()
        .map(|l| {
            let external = if l.url.starts_with('/') {
                ""
            } else {
                " rel=\"noopener noreferrer\""
            };
            format!(
                "<a href=\"{}\"{external}>{}</a>",
                escape(&l.url),
                escape(&l.label)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let title = if b.tagline.is_empty() {
        b.name.clone()
    } else {
        format!("{}: {}", b.name, b.tagline)
    };
    // Escape markup in the JSON so it cannot close the script element.
    let data = public_json(config)
        .to_string()
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('&', "\\u0026");
    let logo = if b.logo_url.is_empty() {
        BUILTIN_LOGO
    } else {
        &b.logo_url
    };
    [
        ("{{favicon_links}}", favicon),
        ("{{footer_links}}", links),
        ("{{branding_json}}", data),
        ("{{title}}", escape(&title)),
        ("{{name}}", escape(&b.name)),
        ("{{description}}", escape(&b.description)),
        ("{{theme_color}}", escape(&b.theme_color)),
        (
            "{{color_scheme}}",
            match b.color_scheme.as_str() {
                "system" => "dark light".to_owned(),
                scheme => escape(scheme),
            },
        ),
        ("{{logo_url}}", escape(logo)),
        ("{{footer_text}}", escape(&b.footer_text)),
    ]
    .iter()
    .fold(template.to_owned(), |page, (placeholder, value)| {
        page.replace(placeholder, value)
    })
}

/// Reads a file from the theme directory when it exists and is small enough.
async fn theme_file(app: &App, relative: &Path, limit: u64) -> Option<Vec<u8>> {
    let root = tokio::fs::canonicalize(app.theme_dir.as_deref()?)
        .await
        .ok()?;
    let path = tokio::fs::canonicalize(root.join(relative)).await.ok()?;
    // Symlinks must not lead outside the theme directory.
    if !path.starts_with(&root) {
        return None;
    }
    let meta = tokio::fs::metadata(&path).await.ok()?;
    if !meta.is_file() || meta.len() > limit {
        return None;
    }
    tokio::fs::read(&path).await.ok()
}

/// Returns the HTML shell for `/`, `/workspace/`, and `/admin/`.
///
/// Uses the theme's `index.html` when one exists, and the built-in template
/// otherwise.
pub async fn page(app: &App) -> Html<String> {
    let config = app.config().await;
    let custom = theme_file(app, Path::new("index.html"), MAX_TEMPLATE_BYTES)
        .await
        .and_then(|bytes| String::from_utf8(bytes).ok());
    let mut html = render(custom.as_deref().unwrap_or(BUILTIN_TEMPLATE), &config);
    // A custom favicon gets the conventional companions from the theme
    // directory: a PNG fallback and the iOS home-screen icon.
    let mut extra = String::new();
    if config.branding.favicon_url.starts_with("/theme/") {
        if theme_exists(app, "favicon-32.png").await {
            extra.push_str(
                "\n  <link rel=\"icon\" type=\"image/png\" sizes=\"32x32\" href=\"/theme/favicon-32.png\">",
            );
        }
        if theme_exists(app, "apple-touch-icon.png").await {
            extra.push_str(
                "\n  <link rel=\"apple-touch-icon\" sizes=\"180x180\" href=\"/theme/apple-touch-icon.png\">",
            );
        }
    }
    if !extra.is_empty()
        && let Some(at) = html.find("</head>")
    {
        html.insert_str(at, &format!("{}\n", extra.trim_start_matches('\n')));
    }
    Html(html)
}

/// Whether the theme directory holds a regular file `name`.
async fn theme_exists(app: &App, name: &str) -> bool {
    match &app.theme_dir {
        Some(dir) => tokio::fs::metadata(dir.join(name))
            .await
            .is_ok_and(|m| m.is_file()),
        None => false,
    }
}

/// `/assets/theme.css`: branding variables, custom CSS, then `theme.css`
/// from the theme directory.
pub async fn stylesheet(app: &App) -> Response {
    let mut css = app.config().await.branding.stylesheet();
    if let Some(extra) = theme_file(app, Path::new("theme.css"), MAX_THEME_FILE_BYTES).await {
        css.push_str("\n/* theme.css */\n");
        css.push_str(&String::from_utf8_lossy(&extra));
    }
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], css).into_response()
}

/// Serves `/theme/<path>` from the theme directory.
///
/// Only plain relative paths with an allowed extension are served. Each path
/// segment may use ASCII letters, digits, `.`, `_`, and `-`. Dot-files and
/// traversal are rejected.
pub async fn file(app: &App, path: &str) -> Response {
    let not_found = || StatusCode::NOT_FOUND.into_response();
    let relative = Path::new(path);
    let safe = !path.is_empty()
        && relative.components().all(|c| {
            matches!(c, Component::Normal(s) if s.to_str().is_some_and(|s| {
                !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            }))
        });
    let Some(mime) = content_type(relative).filter(|_| safe) else {
        return not_found();
    };
    let Some(bytes) = theme_file(app, relative, MAX_THEME_FILE_BYTES).await else {
        return not_found();
    };
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Branding, Link};

    #[test]
    fn template_values_are_escaped() {
        let b = Branding {
            name: "Acme <AI>".into(),
            tagline: String::new(),
            logo_url: "/theme/logo.svg".into(),
            favicon_url: "/theme/icon.png".into(),
            hero_title: "</script><script>alert(1)</script>".into(),
            footer_links: vec![Link {
                label: "Docs & help".into(),
                url: "https://docs.example.com/?a=1&b=2".into(),
            }],
            ..Branding::default()
        };
        let config = Config {
            branding: b,
            ..Config::default()
        };
        let page = render(BUILTIN_TEMPLATE, &config);
        assert!(page.contains("<title>Acme &lt;AI&gt;</title>"));
        assert!(page.contains("src=\"/theme/logo.svg\""));
        assert!(page.contains("<link rel=\"icon\" type=\"image/png\" href=\"/theme/icon.png\">"));
        assert!(page.contains("Docs &amp; help"));
        assert!(page.contains("a=1&amp;b=2"));
        assert!(!page.contains("</script><script>alert"));
        assert!(!page.contains("{{"));
        // The default theme follows the device, and the app applies it.
        assert!(page.contains("name=\"color-scheme\" content=\"dark light\""));
        assert!(page.contains("\"request_estimate_usd\":\"0.001\""));
    }
}
