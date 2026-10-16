use quick_xml::events::attributes::Attribute;
use quick_xml::events::{BytesRef, BytesText, Event};
use quick_xml::{Reader, XmlVersion};
use std::io::BufRead;

/// Event source that folds adjacent text, entity references and CDATA into one
/// text event and skips whitespace-only runs. quick-xml no longer unescapes
/// text itself; entities arrive as separate `GeneralRef` events.
pub struct XmlEvents<R: BufRead> {
    reader: Reader<R>,
    buf: Vec<u8>,
    held: Option<Event<'static>>,
}

impl<R: BufRead> XmlEvents<R> {
    pub fn new(source: R) -> Self {
        Self { reader: Reader::from_reader(source), buf: Vec::new(), held: None }
    }

    /// Next event; `Eof` also stands in for a read error.
    pub fn next(&mut self) -> Event<'static> {
        if let Some(ev) = self.held.take() {
            return ev;
        }
        let mut text = String::new();
        loop {
            self.buf.clear();
            let ev = match self.reader.read_event_into(&mut self.buf) {
                Ok(Event::Text(t)) => {
                    text.push_str(&t.xml_content(XmlVersion::Implicit1_0));
                    continue;
                }
                Ok(Event::CData(c)) => {
                    text.push_str(&c.xml_content(XmlVersion::Implicit1_0));
                    continue;
                }
                Ok(Event::GeneralRef(r)) => {
                    if let Some(s) = resolve_ref(&r) {
                        text.push_str(&s);
                    }
                    continue;
                }
                Ok(other) => other.into_owned(),
                Err(_) => Event::Eof,
            };
            return self.deliver(ev, text);
        }
    }

    fn deliver(&mut self, ev: Event<'static>, text: String) -> Event<'static> {
        if text.trim().is_empty() {
            return ev;
        }
        self.held = Some(ev);
        Event::Text(BytesText::from_escaped(text).into_owned())
    }
}

pub fn attr_value(attr: &Attribute) -> String {
    attr.normalized_value(XmlVersion::Implicit1_0).map(|v| v.into_owned()).unwrap_or_default()
}

fn resolve_ref(r: &BytesRef) -> Option<String> {
    if r.is_char_ref() {
        return r.resolve_char_ref().ok().flatten().map(String::from);
    }
    match r.as_ref() {
        "amp" => Some("&".into()),
        "lt" => Some("<".into()),
        "gt" => Some(">".into()),
        "quot" => Some("\"".into()),
        "apos" => Some("'".into()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(xml: &str) -> Vec<String> {
        let mut ev = XmlEvents::new(xml.as_bytes());
        let mut out = Vec::new();
        loop {
            match ev.next() {
                Event::Text(t) => out.push(t.xml_content(XmlVersion::Implicit1_0).into_owned()),
                Event::Eof => return out,
                _ => {}
            }
        }
    }

    #[test]
    fn folds_entities_into_one_text_event() {
        assert_eq!(texts("<a>Tom &amp; Jerry &lt;3 &#x41;&#66;</a>"), ["Tom & Jerry <3 AB"]);
    }

    #[test]
    fn merges_text_and_cdata() {
        assert_eq!(texts("<a>one <![CDATA[<b>two</b>]]></a>"), ["one <b>two</b>"]);
    }

    #[test]
    fn skips_whitespace_between_tags() {
        assert_eq!(texts("<a>\n  <b>x</b>\n  <c>y</c>\n</a>"), ["x", "y"]);
    }

    #[test]
    fn delivers_following_event_after_text() {
        let mut ev = XmlEvents::new("<a>hi</a>".as_bytes());
        assert!(matches!(ev.next(), Event::Start(_)));
        assert!(matches!(ev.next(), Event::Text(_)));
        assert!(matches!(ev.next(), Event::End(_)));
        assert!(matches!(ev.next(), Event::Eof));
    }
}
