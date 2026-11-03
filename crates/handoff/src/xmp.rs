//! The XMP packet SilverCraft embeds in each DNG and writes beside it as `<stem>.xmp`.
//!
//! Standard properties LightCraft imports (title, description, keywords, film-camera make/model,
//! capture date), an optional complete LightCraft develop document in `lc:settings`, and
//! SilverCraft's own provenance in the `sc:` namespace. Our data never goes under `lc:`:
//! LightCraft owns that namespace and replaces it whenever it rewrites a sidecar, while unknown
//! namespaces are kept byte for byte.

pub const SC_NS: &str = "http://ns.silvercraft.app/sc/1.0/";
pub const LC_NS: &str = "http://ns.lightcraft.app/lc/1.0/";

/// Everything the packet can carry; empty fields are left out.
#[derive(Clone, Debug, Default)]
pub struct XmpPacket {
    pub title: Option<String>,
    pub description: Option<String>,
    pub keywords: Vec<String>,
    /// Film camera, when known.
    pub make: Option<String>,
    pub model: Option<String>,
    /// ISO 8601 date/time the frame was shot, when known (not the scan time).
    pub date_shot: Option<String>,
    /// A complete LightCraft `DevelopSettings` JSON document.
    pub lc_settings: Option<String>,
    /// `sc:` simple properties, written in this order (names are local parts, e.g. `roll`).
    pub sc: Vec<(String, String)>,
    pub creator_tool: Option<String>,
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters even escaped
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => out.push(c),
        }
    }
    out
}

/// An XMP-safe local name (letters, digits, `_`, `-`, `.`; starts with a letter or `_`).
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_') && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

impl XmpPacket {
    pub fn to_xml(&self) -> String {
        let mut attrs = String::new();
        let mut elems = String::new();
        let mut attr = |name: &str, value: &Option<String>| {
            if let Some(v) = value {
                attrs.push_str(&format!("\n   {name}=\"{}\"", escape(v)));
            }
        };
        attr("xmp:CreatorTool", &self.creator_tool);
        attr("tiff:Make", &self.make);
        attr("tiff:Model", &self.model);
        attr("exif:DateTimeOriginal", &self.date_shot);
        attr("lc:settings", &self.lc_settings);
        for (k, v) in &self.sc {
            if valid_name(k) {
                attrs.push_str(&format!("\n   sc:{k}=\"{}\"", escape(v)));
            }
        }
        let alt = |name: &str, value: &Option<String>| match value {
            Some(v) => {
                format!("\n   <{name}>\n    <rdf:Alt>\n     <rdf:li xml:lang=\"x-default\">{}</rdf:li>\n    </rdf:Alt>\n   </{name}>", escape(v))
            }
            None => String::new(),
        };
        elems.push_str(&alt("dc:title", &self.title));
        elems.push_str(&alt("dc:description", &self.description));
        if !self.keywords.is_empty() {
            elems.push_str("\n   <dc:subject>\n    <rdf:Bag>");
            for k in &self.keywords {
                elems.push_str(&format!("\n     <rdf:li>{}</rdf:li>", escape(k)));
            }
            elems.push_str("\n    </rdf:Bag>\n   </dc:subject>");
        }
        format!(
            "<?xpacket begin=\"\u{feff}\" id=\"W5M0MpCehiHzreSzNTczkc9d\"?>\n\
             <x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\n \
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\n  \
             <rdf:Description rdf:about=\"\"\n   \
             xmlns:dc=\"http://purl.org/dc/elements/1.1/\"\n   \
             xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\"\n   \
             xmlns:tiff=\"http://ns.adobe.com/tiff/1.0/\"\n   \
             xmlns:exif=\"http://ns.adobe.com/exif/1.0/\"\n   \
             xmlns:lc=\"{LC_NS}\"\n   \
             xmlns:sc=\"{SC_NS}\"{attrs}>{elems}\n  \
             </rdf:Description>\n \
             </rdf:RDF>\n\
             </x:xmpmeta>\n\
             <?xpacket end=\"w\"?>\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> XmpPacket {
        XmpPacket {
            title: Some("Roll 042 · 17".into()),
            description: Some("Bridge <at> dusk & \"fog\"".into()),
            keywords: vec!["film:Kodak Portra 400".into(), "roll:042".into(), "SilverCraft".into()],
            make: Some("Leica".into()),
            model: Some("M6".into()),
            date_shot: Some("2026-09-01T18:30:00".into()),
            lc_settings: None,
            sc: vec![("roll".into(), "042".into()), ("frame".into(), "17".into()), ("provenance".into(), "{\"base\":[0.8,0.5,0.3]}".into())],
            creator_tool: Some("SilverCraft 0.1.0".into()),
        }
    }

    #[test]
    fn lightcraft_reads_the_standard_fields() {
        let xml = sample().to_xml();
        let d = lightcraft_meta_parse(&xml);
        let m = &d.metadata;
        assert_eq!(m.title.as_deref(), Some("Roll 042 · 17"));
        assert_eq!(m.caption.as_deref(), Some("Bridge <at> dusk & \"fog\""));
        assert_eq!(m.keywords, vec!["film:Kodak Portra 400", "roll:042", "SilverCraft"]);
        assert_eq!(m.make.as_deref(), Some("Leica"));
        assert_eq!(m.model.as_deref(), Some("M6"));
        assert!(m.capture_time.is_some());
        // our namespace survives as plain properties
        assert_eq!(d.properties.get("sc:roll").and_then(|v| v.first()).map(String::as_str), Some("042"));
        assert_eq!(d.properties.get("sc:provenance").and_then(|v| v.first()).map(String::as_str), Some("{\"base\":[0.8,0.5,0.3]}"));
    }

    #[test]
    fn lc_settings_round_trip_and_bad_names_are_dropped() {
        let mut p = sample();
        p.lc_settings = Some("{\"light\":{\"exposure\":0.5}}".into());
        p.sc.push(("bad name\"/>".into(), "x".into()));
        let xml = p.to_xml();
        assert!(!xml.contains("bad name"));
        let d = lightcraft_meta_parse(&xml);
        assert_eq!(d.lc_settings.as_deref(), Some("{\"light\":{\"exposure\":0.5}}"));
    }

    #[test]
    fn empty_packet_is_valid() {
        let d = lightcraft_meta_parse(&XmpPacket::default().to_xml());
        assert!(d.metadata.keywords.is_empty());
    }

    fn lightcraft_meta_parse(xml: &str) -> lightcraft_meta::XmpData {
        lightcraft_meta::parse_xmp(xml).unwrap()
    }
}
