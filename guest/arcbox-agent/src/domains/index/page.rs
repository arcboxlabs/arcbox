//! HTML for the current, concrete web service directory.

use std::fmt::Write as _;

use arcbox_dns::web_services::WebServiceUrls;

pub(super) fn render(services: Option<&[WebServiceUrls]>) -> String {
    let mut page = String::from(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>ArcBox local services</title><style>\
         :root{color-scheme:light dark;font:16px system-ui,sans-serif}\
         body{max-width:54rem;margin:4rem auto;padding:0 1.5rem}\
         h1{font-size:2rem}p{line-height:1.5}li{margin:1.5rem 0}\
         a{margin-right:1rem}small{display:block;margin-top:.5rem;opacity:.7}\
         </style><body><h1>Local web services</h1>",
    );
    match services {
        None => page.push_str(
            "<p>The container directory is unavailable. Refresh after the runtime reconnects.</p>",
        ),
        Some([]) => page.push_str("<p>No running web services are available.</p>"),
        Some(services) => {
            page.push_str("<ul>");
            for service in services {
                write!(page, "<li><p>{}</p>", Escaped(&service.hostname)).unwrap();
                for (label, url) in [("HTTP", &service.http_url), ("HTTPS", &service.https_url)] {
                    if !url.is_empty() {
                        write!(page, "<a href=\"{}\">{label}</a>", Escaped(url)).unwrap();
                    }
                }
                if service.domains.len() > 1 {
                    let aliases = service.domains.join(", ");
                    write!(page, "<small>{}</small>", Escaped(&aliases)).unwrap();
                }
                page.push_str("</li>");
            }
            page.push_str("</ul>");
        }
    }
    page.push_str("</body></html>");
    page
}

struct Escaped<'a>(&'a str);

impl std::fmt::Display for Escaped<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for character in self.0.chars() {
            match character {
                '&' => formatter.write_str("&amp;")?,
                '<' => formatter.write_str("&lt;")?,
                '>' => formatter.write_str("&gt;")?,
                '"' => formatter.write_str("&quot;")?,
                '\'' => formatter.write_str("&#39;")?,
                _ => formatter.write_char(character)?,
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory_escapes_text_and_attributes_and_omits_unavailable_protocols() {
        let service = WebServiceUrls {
            hostname: "<script>".into(),
            domains: vec!["<script>".into(), "a&b".into()],
            http_url: "http://host/?q=\"&'".into(),
            https_url: String::new(),
            https_mode: arcbox_dns::web_services::HttpsMode::None,
        };
        let page = render(Some(&[service]));
        assert!(page.contains("&lt;script&gt;"));
        assert!(page.contains("a&amp;b"));
        assert!(page.contains("href=\"http://host/?q=&quot;&amp;&#39;\""));
        assert!(!page.contains("<script>"));
        assert!(!page.contains(">HTTPS</a>"));
    }

    #[test]
    fn source_failure_and_empty_directory_have_no_clickable_links() {
        let unavailable = render(None);
        assert!(unavailable.contains("directory is unavailable"));
        assert!(!unavailable.contains("href="));
        let empty = render(Some(&[]));
        assert!(empty.contains("No running web services"));
        assert!(!empty.contains("href="));
    }
}
