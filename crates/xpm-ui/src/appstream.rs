use crate::xml::{attr_value, XmlEvents};
use quick_xml::events::Event;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Default)]
pub(crate) struct Screenshot {
    pub url: String,
    pub source_url: String,
    pub caption: String,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub(crate) struct Release {
    pub version: String,
    pub date: String,
    pub notes: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub(crate) struct CachedRemoteApp {
    pub app_id: String,
    pub name: String,
    pub summary: String,
    pub description: String,
    pub categories: Vec<String>,
    pub developer: String,
    pub screenshot_url: String,
    #[serde(default)]
    pub icon_name: String,
    #[serde(default)]
    pub extends: String,
    #[serde(default)]
    pub url_homepage: String,
    #[serde(default)]
    pub url_bugtracker: String,
    #[serde(default)]
    pub url_translate: String,
    #[serde(default)]
    pub url_vcs: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub version_date: String,
    #[serde(default)]
    pub changelog: String,
    #[serde(default)]
    pub screenshots: Vec<Screenshot>,
    #[serde(default)]
    pub license: String,
    #[serde(default)]
    pub releases: Vec<Release>,
    #[serde(default)]
    pub url_donation: String,
    #[serde(default)]
    pub url_help: String,
    #[serde(default)]
    pub url_faq: String,
}

/// Strip residual HTML tags (e.g. `<em>` left after unescaping) from description text.
pub(crate) fn strip_inline_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let mut result = String::new();
    let mut blank_run = 0usize;
    for line in out.split('\n') {
        let t = line.trim();
        if t.is_empty() {
            blank_run += 1;
            if blank_run == 1 {
                result.push('\n');
            }
        } else {
            blank_run = 0;
            result.push_str(t);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn civil_date(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

fn release_attrs(e: &quick_xml::events::BytesStart) -> (String, String) {
    let (mut version, mut date, mut timestamp) = (String::new(), String::new(), String::new());
    for attr in e.attributes().flatten() {
        match attr.key.as_ref() {
            "version" => version = attr_value(&attr),
            "date" => date = attr_value(&attr),
            "timestamp" => timestamp = attr_value(&attr),
            _ => {}
        }
    }
    if date.is_empty() && let Ok(secs) = timestamp.parse::<i64>() {
        date = civil_date(secs);
    }
    (version, date)
}

/// Parse an AppStream collection (as shipped by flatpak remotes) into app entries.
pub(crate) fn parse(xml_bytes: &[u8]) -> Vec<CachedRemoteApp> {
    struct State {
        app_id: String,
        name: String,
        summary: String,
        description: String,
        categories: Vec<String>,
        developer: String,
        screenshot_url: String,
        screenshot_source_url: String,
        icon_name: String,
        extends: String,
        url_homepage: String,
        url_bugtracker: String,
        url_translate: String,
        url_vcs: String,
        version: String,
        version_date: String,
        changelog: String,
        screenshots: Vec<Screenshot>,
        license: String,
        releases: Vec<Release>,
        url_donation: String,
        url_help: String,
        url_faq: String,
    }

    let mut current: Option<State> = None;
    let mut apps: Vec<CachedRemoteApp> = Vec::new();

    let mut in_component = false;
    let mut in_id = false;
    let mut in_name = false;
    let mut in_summary = false;
    let mut in_description = false;
    let mut desc_depth: i32 = 0;
    let mut in_developer = false;
    let mut in_developer_name = false;
    let mut in_categories = false;
    let mut in_category = false;
    let mut in_screenshots = false;
    let mut in_screenshot = false;
    let mut cur_image_type = String::new();
    let mut in_image = false;
    let mut in_extends = false;
    let mut in_icon = false;
    let mut in_url = false;
    let mut cur_url_type = String::new();
    let mut in_releases = false;
    let mut in_release = false;
    let mut in_license = false;
    let mut in_caption = false;
    let mut cur_image_width: u32 = 0;
    let mut shot_thumb: Option<(u32, String)> = None;
    let mut shot_source = String::new();
    let mut shot_caption = String::new();
    let mut rel_version = String::new();
    let mut rel_date = String::new();
    let mut rel_notes = String::new();
    let mut in_release_desc = false;
    let mut release_desc_depth: i32 = 0;

    let mut events = XmlEvents::new(xml_bytes);

    let mut skip_depth = 0u32;

    loop {
        let event = events.next();
        if skip_depth > 0 {
            match event {
                Event::Start(_) => skip_depth += 1,
                Event::End(_) => skip_depth -= 1,
                Event::Eof => break,
                _ => {}
            }
            continue;
        }
        match event {
            Event::Start(e) => {
                if in_component && e.attributes().flatten().any(|a| a.key.as_ref() == "xml:lang") {
                    skip_depth = 1;
                    continue;
                }
                let tag = e.name();
                match tag.as_ref() {
                    "component" => {
                        in_component = true;
                        current = Some(State {
                            app_id: String::new(),
                            name: String::new(),
                            summary: String::new(),
                            description: String::new(),
                            categories: Vec::new(),
                            developer: String::new(),
                            screenshot_url: String::new(),
                            screenshot_source_url: String::new(),
                            icon_name: String::new(),
                            extends: String::new(),
                            url_homepage: String::new(),
                            url_bugtracker: String::new(),
                            url_translate: String::new(),
                            url_vcs: String::new(),
                            version: String::new(),
                            version_date: String::new(),
                            changelog: String::new(),
                            screenshots: Vec::new(),
                            license: String::new(),
                            releases: Vec::new(),
                            url_donation: String::new(),
                            url_help: String::new(),
                            url_faq: String::new(),
                        });
                    }
                    "id" if in_component && !in_developer && !in_description && !in_categories && !in_screenshots => {
                        in_id = true;
                    }
                    "name" if in_component && !in_developer && !in_description && !in_categories => {
                        in_name = true;
                    }
                    "summary" if in_component && !in_developer && !in_description => {
                        in_summary = true;
                    }
                    "description" if in_component && !in_screenshots && !in_releases => {
                        in_description = true;
                        desc_depth = 1;
                    }
                    "url" if in_component && !in_description && !in_screenshots && !in_releases => {
                        cur_url_type.clear();
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == "type" {
                                cur_url_type = attr_value(&attr);
                            }
                        }
                        in_url = true;
                    }
                    "releases" if in_component => { in_releases = true; }
                    "release" if in_releases => {
                        rel_version.clear();
                        rel_date.clear();
                        rel_notes.clear();
                        (rel_version, rel_date) = release_attrs(&e);
                        in_release = true;
                    }
                    "description" if in_release && !in_release_desc => {
                        in_release_desc = true;
                        release_desc_depth = 1;
                    }
                    "li" if in_release_desc => {
                        if !rel_notes.is_empty() && !rel_notes.ends_with('\n') {
                            rel_notes.push('\n');
                        }
                        rel_notes.push_str("• ");
                        release_desc_depth += 1;
                    }
                    _ if in_release_desc => { release_desc_depth += 1; }
                    "project_license" if in_component => { in_license = true; }
                    "developer" if in_component => {
                        in_developer = true;
                    }
                    "name" if in_developer => {
                        in_developer_name = true;
                    }
                    "categories" if in_component => {
                        in_categories = true;
                    }
                    "category" if in_categories => {
                        in_category = true;
                    }
                    "extends" if in_component && !in_description => {
                        in_extends = true;
                    }
                    "screenshots" if in_component => {
                        in_screenshots = true;
                    }
                    "screenshot" if in_screenshots => {
                        in_screenshot = true;
                        shot_thumb = None;
                        shot_source.clear();
                        shot_caption.clear();
                    }
                    "caption" if in_screenshot => {
                        in_caption = true;
                    }
                    "image" if in_screenshot => {
                        in_image = true;
                        cur_image_type.clear();
                        cur_image_width = 0;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                "type" => { cur_image_type = attr_value(&attr); }
                                "width" => { cur_image_width = attr_value(&attr).parse().unwrap_or(0); }
                                _ => {}
                            }
                        }
                    }
                    "icon" if in_component && !in_screenshots && !in_description => {
                        let mut is_cached = false;
                        let mut is_128 = false;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                "type" => { is_cached = attr_value(&attr) == "cached"; }
                                "width" => { is_128 = attr_value(&attr) == "128"; }
                                _ => {}
                            }
                        }
                        if is_cached && is_128 {
                            if let Some(ref mut s) = current {
                                let _ = s;
                            }
                            in_icon = true;
                        }
                    }
                    "li" if in_description => {
                        if let Some(ref mut state) = current {
                            if !state.description.is_empty() && !state.description.ends_with('\n') {
                                state.description.push('\n');
                            }
                            state.description.push_str("• ");
                        }
                        desc_depth += 1;
                    }
                    _ if in_description => {
                        desc_depth += 1;
                    }
                    _ => {}
                }
            }
            Event::End(e) => {
                let tag = e.name();
                match tag.as_ref() {
                    "component" => {
                        in_component = false;
                        if let Some(state) = current.take()
                            && !state.app_id.is_empty() {
                                let ss_url = if !state.screenshot_url.is_empty() {
                                    state.screenshot_url
                                } else {
                                    state.screenshot_source_url
                                };
                                apps.push(CachedRemoteApp {
                                    app_id: state.app_id,
                                    name: state.name,
                                    summary: state.summary,
                                    description: strip_inline_tags(&state.description),
                                    categories: state.categories,
                                    developer: state.developer,
                                    screenshot_url: ss_url,
                                    icon_name: state.icon_name,
                                    extends: state.extends,
                                    url_homepage: state.url_homepage,
                                    url_bugtracker: state.url_bugtracker,
                                    url_translate: state.url_translate,
                                    url_vcs: state.url_vcs,
                                    version: state.version,
                                    version_date: state.version_date,
                                    changelog: strip_inline_tags(&state.changelog),
                                    screenshots: state.screenshots,
                                    license: state.license,
                                    releases: state.releases,
                                    url_donation: state.url_donation,
                                    url_help: state.url_help,
                                    url_faq: state.url_faq,
                                });
                            }
                    }
                    "id" => { in_id = false; }
                    "extends" => { in_extends = false; }
                    "name" if in_developer => { in_developer_name = false; }
                    "name" if !in_developer => { in_name = false; }
                    "summary" => { in_summary = false; }
                    "description" if desc_depth == 1 => { in_description = false; desc_depth = 0; }
                    "developer" => { in_developer = false; in_developer_name = false; }
                    "categories" => { in_categories = false; }
                    "category" => { in_category = false; }
                    "screenshots" => { in_screenshots = false; }
                    "screenshot" => {
                        in_screenshot = false;
                        let url = shot_thumb.take().map(|(_, u)| u).filter(|u| !u.is_empty())
                            .unwrap_or_else(|| shot_source.clone());
                        if !url.is_empty()
                            && let Some(ref mut state) = current
                            && state.screenshots.len() < 12 {
                                state.screenshots.push(Screenshot {
                                    url,
                                    source_url: std::mem::take(&mut shot_source),
                                    caption: std::mem::take(&mut shot_caption),
                                });
                            }
                    }
                    "caption" => { in_caption = false; }
                    "image" => { in_image = false; }
                    "project_license" => { in_license = false; }
                    "icon" => { in_icon = false; }
                    "url" => { in_url = false; cur_url_type.clear(); }
                    "releases" => { in_releases = false; }
                    "release" if in_release => {
                        in_release = false;
                        if let Some(ref mut state) = current {
                            let notes = strip_inline_tags(&rel_notes);
                            if state.releases.is_empty() {
                                state.version = rel_version.clone();
                                state.version_date = rel_date.clone();
                                state.changelog = rel_notes.clone();
                            }
                            if state.releases.len() < 5 {
                                state.releases.push(Release {
                                    version: rel_version.clone(),
                                    date: rel_date.clone(),
                                    notes,
                                });
                            }
                        }
                    }
                    "description" if in_release_desc && release_desc_depth == 1 => {
                        in_release_desc = false;
                        release_desc_depth = 0;
                    }
                    "p" if in_release_desc => {
                        if !rel_notes.is_empty() {
                            if !rel_notes.ends_with('\n') { rel_notes.push('\n'); }
                            rel_notes.push('\n');
                        }
                        release_desc_depth -= 1;
                    }
                    "li" if in_release_desc => {
                        if !rel_notes.ends_with('\n') { rel_notes.push('\n'); }
                        release_desc_depth -= 1;
                    }
                    "ul" | "ol" if in_release_desc => { release_desc_depth -= 1; }
                    _ if in_release_desc => { release_desc_depth -= 1; }
                    "p" if in_description => {
                        if let Some(ref mut state) = current
                            && !state.description.is_empty() {
                                if !state.description.ends_with('\n') {
                                    state.description.push('\n');
                                }
                                state.description.push('\n');
                            }
                        desc_depth -= 1;
                    }
                    "li" if in_description => {
                        if let Some(ref mut state) = current
                            && !state.description.ends_with('\n') {
                                state.description.push('\n');
                            }
                        desc_depth -= 1;
                    }
                    "ul" | "ol" if in_description => {
                        if let Some(ref mut state) = current
                            && !state.description.ends_with("\n\n") {
                                if !state.description.ends_with('\n') {
                                    state.description.push('\n');
                                }
                                state.description.push('\n');
                            }
                        desc_depth -= 1;
                    }
                    _ if in_description => { desc_depth -= 1; }
                    _ => {}
                }
            }
            Event::Text(e) => {
                let text = e.xml_content(quick_xml::XmlVersion::Implicit1_0).into_owned();
                if let Some(ref mut state) = current {
                    if in_id && state.app_id.is_empty() { state.app_id = text.trim().to_string(); }
                    else if in_extends && state.extends.is_empty() { state.extends = text.trim().to_string(); }
                    else if in_name && state.name.is_empty() { state.name = text.trim().to_string(); }
                    else if in_summary && state.summary.is_empty() { state.summary = text.trim().to_string(); }
                    else if in_description {
                        let collapsed = collapse_whitespace(&text);
                        let t = collapsed.trim();
                        if !t.is_empty() {
                            if !state.description.is_empty()
                                && !state.description.ends_with('\n')
                                && !state.description.ends_with(' ')
                            {
                                state.description.push(' ');
                            }
                            state.description.push_str(t);
                        }
                    } else if in_url {
                        let url_text = text.trim().to_string();
                        match cur_url_type.as_str() {
                            "homepage" if state.url_homepage.is_empty() => { state.url_homepage = url_text; }
                            "bugtracker" if state.url_bugtracker.is_empty() => { state.url_bugtracker = url_text; }
                            "translate" if state.url_translate.is_empty() => { state.url_translate = url_text; }
                            "vcs-browser" if state.url_vcs.is_empty() => { state.url_vcs = url_text; }
                            "donation" if state.url_donation.is_empty() => { state.url_donation = url_text; }
                            "help" if state.url_help.is_empty() => { state.url_help = url_text; }
                            "faq" if state.url_faq.is_empty() => { state.url_faq = url_text; }
                            _ => {}
                        }
                    } else if in_release_desc {
                        let collapsed = collapse_whitespace(&text);
                        let t = collapsed.trim();
                        if !t.is_empty() {
                            if !rel_notes.is_empty()
                                && !rel_notes.ends_with('\n')
                                && !rel_notes.ends_with(' ')
                            {
                                rel_notes.push(' ');
                            }
                            rel_notes.push_str(t);
                        }
                    } else if in_license && state.license.is_empty() {
                        state.license = text.trim().to_string();
                    } else if in_caption && shot_caption.is_empty() {
                        shot_caption = text.trim().to_string();
                    } else if in_developer_name && state.developer.is_empty() {
                        state.developer = text.trim().to_string();
                    } else if in_category {
                        state.categories.push(text.trim().to_string());
                    } else if in_icon && state.icon_name.is_empty() {
                        state.icon_name = text.trim().to_string();
                    } else if in_image && in_screenshot {
                        let url = text.trim().to_string();
                        if cur_image_type == "thumbnail" {
                            let better = match &shot_thumb {
                                None => true,
                                Some((w, _)) => {
                                    let (cur, new) = (*w, cur_image_width);
                                    if new >= 600 { cur < 600 || new < cur } else { cur < 600 && new > cur }
                                }
                            };
                            if better { shot_thumb = Some((cur_image_width, url.clone())); }
                        } else if cur_image_type == "source" {
                            shot_source = url.clone();
                        }
                        if cur_image_type == "thumbnail"
                            && state.screenshot_url.is_empty()
                            && url.contains("624x351")
                        {
                            state.screenshot_url = url;
                        } else if cur_image_type == "source"
                            && state.screenshot_source_url.is_empty()
                        {
                            state.screenshot_source_url = url;
                        }
                    }
                }
            }
            Event::Empty(e) if e.name().as_ref() == "release" && in_releases => {
                if let Some(ref mut state) = current {
                    let (version, date) = release_attrs(&e);
                    if state.releases.is_empty() {
                        state.version = version.clone();
                        state.version_date = date.clone();
                    }
                    if state.releases.len() < 5 {
                        state.releases.push(Release { version, date, notes: String::new() });
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }

    apps
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<components>
<component type="desktop-application">
  <id>org.example.App</id>
  <name>Example</name>
  <name xml:lang="de">Beispiel</name>
  <summary>Does things</summary>
  <project_license>GPL-3.0-or-later</project_license>
  <developer><name>Example Team</name></developer>
  <url type="homepage">https://example.org</url>
  <url type="donation">https://example.org/donate</url>
  <url type="help">https://example.org/help</url>
  <screenshots>
    <screenshot type="default">
      <caption>Main window</caption>
      <caption xml:lang="de">Hauptfenster</caption>
      <image type="source" width="1920" height="1080">https://img/one-src.png</image>
      <image type="thumbnail" width="112" height="63">https://img/one-112.png</image>
      <image type="thumbnail" width="752" height="423">https://img/one-752.png</image>
      <image type="thumbnail" width="1504" height="846">https://img/one-1504.png</image>
    </screenshot>
    <screenshot>
      <image type="thumbnail" width="224" height="126">https://img/two-224.png</image>
      <image type="thumbnail" width="112" height="63">https://img/two-112.png</image>
    </screenshot>
    <screenshot>
      <image type="source">https://img/three-src.png</image>
    </screenshot>
  </screenshots>
  <releases>
    <release timestamp="1790985600" type="stable" version="2.0"/>
    <release timestamp="951782400" type="stable" version="1.9">
      <description><p>Fixes</p><ul><li>One</li><li>Two</li></ul></description>
    </release>
  </releases>
</component>
</components>"#;

    #[test]
    fn joins_source_line_wraps_inside_paragraphs() {
        let xml = "<components><component type=\"desktop-application\"><id>a.b.C</id>\n<description>\n  <p>First line\n    wraps here.</p>\n  <p>Second paragraph.</p>\n  <ul><li>One\n item</li><li>Two</li></ul>\n</description></component></components>";
        let app = &parse(xml.as_bytes())[0];
        assert_eq!(app.description, "First line wraps here.\n\nSecond paragraph.\n\n• One item\n• Two");
    }

    #[test]
    fn ignores_translated_elements_everywhere() {
        let xml = r#"<components><component type="desktop-application">
<id>a.b.C</id><name>Plain</name><name xml:lang="be">Простое</name>
<summary>Sum</summary><summary xml:lang="de">Zusammenfassung</summary>
<description><p>English text</p></description>
<description xml:lang="be"><p>Беларускі тэкст</p><ul><li>x</li></ul></description>
<developer><name>Dev Team</name><name xml:lang="be">Каманда</name></developer>
<releases><release version="1.0" timestamp="0">
<description><p>Notes</p></description><description xml:lang="de"><p>Notizen</p></description>
</release></releases></component></components>"#;
        let app = &parse(xml.as_bytes())[0];
        assert_eq!(app.name, "Plain");
        assert_eq!(app.summary, "Sum");
        assert_eq!(app.description, "English text");
        assert_eq!(app.developer, "Dev Team");
        assert_eq!(app.releases[0].notes, "Notes");
    }

    #[test]
    fn collects_all_screenshots_with_best_thumbnail() {
        let apps = parse(XML.as_bytes());
        let shots = &apps[0].screenshots;
        assert_eq!(shots.len(), 3);
        assert_eq!(shots[0].url, "https://img/one-752.png");
        assert_eq!(shots[0].source_url, "https://img/one-src.png");
        assert_eq!(shots[0].caption, "Main window");
        assert_eq!(shots[1].url, "https://img/two-224.png");
        assert_eq!(shots[2].url, "https://img/three-src.png");
    }

    #[test]
    fn reads_license_links_and_developer() {
        let app = &parse(XML.as_bytes())[0];
        assert_eq!(app.license, "GPL-3.0-or-later");
        assert_eq!(app.developer, "Example Team");
        assert_eq!(app.url_donation, "https://example.org/donate");
        assert_eq!(app.url_help, "https://example.org/help");
        assert_eq!(app.name, "Example");
    }

    #[test]
    fn keeps_self_closing_and_full_releases_in_order() {
        let app = &parse(XML.as_bytes())[0];
        assert_eq!(app.releases.len(), 2);
        assert_eq!(app.releases[0].version, "2.0");
        assert_eq!(app.releases[0].date, "2026-10-03");
        assert!(app.releases[0].notes.is_empty());
        assert_eq!(app.releases[1].date, "2000-02-29");
        assert_eq!(app.releases[1].notes, "Fixes\n\n• One\n• Two");
        assert_eq!(app.version, "2.0");
    }

    #[test]
    fn civil_date_matches_known_epochs() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(1_790_985_600), "2026-10-03");
    }
}
