//! White-label branding: names, copy, logos, favicons, colours, and custom
//! CSS for the embedded web app.
//!
//! Branding is stored with the other settings (and editable in the console).
//! Static files such as logos and fonts can additionally be served from a
//! theme directory (see [`crate::web`]).
use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Largest accepted `custom_css`, in bytes.
pub const MAX_CUSTOM_CSS_BYTES: usize = 64 * 1024;
const MAX_TEXT_CHARS: usize = 300;
const MAX_LINKS: usize = 10;
const MAX_CSS_VARS: usize = 64;

/// A footer or navigation link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    pub label: String,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Branding {
    /// Product name shown in the header, titles, and footer.
    pub name: String,
    /// Short line appended to page titles (`<name> — <tagline>`).
    pub tagline: String,
    /// `<meta name="description">` content.
    pub description: String,
    /// Header logo: a site path (`/theme/logo.svg`) or an http(s) URL. Empty
    /// uses the built-in mark.
    pub logo_url: String,
    /// Favicon: a site path or an http(s) URL. Empty uses the built-in icon.
    pub favicon_url: String,
    /// Browser UI colour (`<meta name="theme-color">`).
    pub theme_color: String,
    /// `dark` or `light`: the colour scheme advertised to the browser.
    pub color_scheme: String,
    /// CSS custom properties without the leading `--` (for example
    /// `accent = "#ff7a1a"`), applied on `:root` after the built-in theme.
    pub css_vars: BTreeMap<String, String>,
    /// Extra stylesheet appended after the built-in theme.
    pub custom_css: String,
    /// Landing page copy. Empty strings keep the built-in text.
    pub hero_eyebrow: String,
    pub hero_title: String,
    pub hero_text: String,
    /// Footer text next to the product name.
    pub footer_text: String,
    pub footer_links: Vec<Link>,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            name: "Reseller".into(),
            tagline: "API workspace".into(),
            description: "Your API workspace. Create keys, track usage, and manage your balance."
                .into(),
            logo_url: String::new(),
            favicon_url: String::new(),
            theme_color: "#0c0e0d".into(),
            color_scheme: "dark".into(),
            css_vars: BTreeMap::new(),
            custom_css: String::new(),
            hero_eyebrow: String::new(),
            hero_title: String::new(),
            hero_text: String::new(),
            footer_text: "independently hosted".into(),
            footer_links: vec![Link {
                label: "Source ↗".into(),
                url: "https://github.com/reseller-rs/reseller".into(),
            }],
        }
    }
}

/// A safe CSS value: no characters that could close a declaration, block,
/// or escape into markup.
pub fn css_value(v: &str) -> bool {
    !v.trim().is_empty()
        && v.len() <= 200
        && !v
            .chars()
            .any(|c| matches!(c, ';' | '{' | '}' | '<' | '>' | '\\' | '"' | '\'') || c.is_control())
}

/// A same-site path (`/…`, not protocol-relative) or an absolute http(s) URL.
pub fn link_target(v: &str) -> bool {
    if v.starts_with('/') {
        return !v.starts_with("//") && !v.contains(['\\', '"', '<', '>', ' ']);
    }
    url::Url::parse(v).is_ok_and(|u| {
        matches!(u.scheme(), "http" | "https") && u.host_str().is_some() && u.username().is_empty()
    })
}

impl Branding {
    pub fn validate(&self) -> anyhow::Result<()> {
        let name = self.name.trim();
        anyhow::ensure!(
            !name.is_empty() && name.chars().count() <= 80,
            "branding.name must be 1-80 characters"
        );
        for (field, value) in [
            ("tagline", &self.tagline),
            ("description", &self.description),
            ("hero_eyebrow", &self.hero_eyebrow),
            ("hero_title", &self.hero_title),
            ("hero_text", &self.hero_text),
            ("footer_text", &self.footer_text),
        ] {
            anyhow::ensure!(
                value.chars().count() <= MAX_TEXT_CHARS,
                "branding.{field} must be at most {MAX_TEXT_CHARS} characters"
            );
        }
        for (field, value) in [
            ("logo_url", &self.logo_url),
            ("favicon_url", &self.favicon_url),
        ] {
            anyhow::ensure!(
                value.is_empty() || link_target(value),
                "branding.{field} must be a site path (/…) or an http(s) URL"
            );
        }
        anyhow::ensure!(
            css_value(&self.theme_color),
            "branding.theme_color must be a CSS colour"
        );
        anyhow::ensure!(
            matches!(self.color_scheme.as_str(), "dark" | "light"),
            "branding.color_scheme must be \"dark\" or \"light\""
        );
        anyhow::ensure!(
            self.css_vars.len() <= MAX_CSS_VARS,
            "branding.css_vars allows at most {MAX_CSS_VARS} entries"
        );
        for (name, value) in &self.css_vars {
            anyhow::ensure!(
                !name.is_empty()
                    && name.len() <= 40
                    && !name.starts_with('-')
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "branding.css_vars name {name:?} must use a-z, 0-9 and '-' (without leading --)"
            );
            css_value(value)
                .then_some(())
                .with_context(|| format!("branding.css_vars.{name} is not a safe CSS value"))?;
        }
        anyhow::ensure!(
            self.custom_css.len() <= MAX_CUSTOM_CSS_BYTES,
            "branding.custom_css must be at most {MAX_CUSTOM_CSS_BYTES} bytes"
        );
        anyhow::ensure!(
            !self.custom_css.to_ascii_lowercase().contains("</style"),
            "branding.custom_css must not contain markup"
        );
        anyhow::ensure!(
            self.footer_links.len() <= MAX_LINKS,
            "branding.footer_links allows at most {MAX_LINKS} links"
        );
        for link in &self.footer_links {
            anyhow::ensure!(
                !link.label.trim().is_empty() && link.label.chars().count() <= 80,
                "branding.footer_links labels must be 1-80 characters"
            );
            anyhow::ensure!(
                link_target(&link.url),
                "branding.footer_links url {:?} must be a site path or http(s) URL",
                link.url
            );
        }
        Ok(())
    }

    /// Generated stylesheet: colour variables followed by custom CSS.
    pub fn stylesheet(&self) -> String {
        let mut css = String::from("/* Generated from branding settings. */\n");
        if !self.css_vars.is_empty() || self.color_scheme == "light" {
            css.push_str(":root {\n");
            css.push_str(&format!("  color-scheme: {};\n", self.color_scheme));
            for (name, value) in &self.css_vars {
                css.push_str(&format!("  --{name}: {};\n", value.trim()));
            }
            css.push_str("}\n");
        }
        if !self.custom_css.is_empty() {
            css.push_str(&self.custom_css);
            css.push('\n');
        }
        css
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid() {
        Branding::default().validate().unwrap();
    }

    #[test]
    fn unsafe_values_are_rejected() {
        let mut b = Branding::default();
        b.css_vars.insert("accent".into(), "red; } body { x".into());
        assert!(b.validate().is_err());
        let mut b = Branding::default();
        b.css_vars.insert("--accent".into(), "red".into());
        assert!(b.validate().is_err());
        let b = Branding {
            logo_url: "//evil.example/logo.png".into(),
            ..Branding::default()
        };
        assert!(b.validate().is_err());
        let b = Branding {
            logo_url: "javascript:alert(1)".into(),
            ..Branding::default()
        };
        assert!(b.validate().is_err());
        let b = Branding {
            custom_css: "</style><script>".into(),
            ..Branding::default()
        };
        assert!(b.validate().is_err());
    }

    #[test]
    fn stylesheet_renders_variables_and_custom_css() {
        let mut b = Branding {
            custom_css: ".brand { letter-spacing: 1px; }".into(),
            ..Branding::default()
        };
        b.css_vars.insert("accent".into(), "#ff7a1a".into());
        let css = b.stylesheet();
        assert!(css.contains("--accent: #ff7a1a;"));
        assert!(css.contains(".brand { letter-spacing: 1px; }"));
    }
}
