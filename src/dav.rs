use chrono::Utc;
use worker::Object;

#[derive(Debug, Clone)]
pub struct DavBuilder {
    creation_date: String,
    get_content_length: Option<String>,
    get_content_type: String,
    get_etag: Option<String>,
    get_last_modified: String,
    resource_type: String,
    href: String,
}
impl DavBuilder {
    pub fn new() -> Self {
        Self {
            creation_date: Utc::now().to_string(),
            get_content_length: None,
            get_content_type: "httpd/unix-directory".to_string(),
            get_etag: None,
            get_last_modified: Utc::now().to_string(),
            resource_type: "<collection />".to_string(),
            href: String::new(),
        }
    }

    pub fn object(mut self, href: impl AsRef<str>, object: Option<&Object>) -> Self {
        let uploaded_time = object
            .map(|o| o.uploaded().to_string())
            .unwrap_or(Utc::now().to_string());

        self.creation_date = uploaded_time.clone();
        self.href = href.as_ref().to_string();

        self.get_content_length = object.map(|o| o.size().to_string());
        self.get_content_type = object
            .and_then(|o| o.http_metadata().content_type)
            .unwrap_or("httpd/unix-directory".to_string());
        self.get_etag = object.map(|o| o.etag());
        self.get_last_modified = uploaded_time;
        self.resource_type = object
            .map(|o| {
                o.custom_metadata()
                    .ok()
                    .and_then(|c| c.get("resource_type").cloned())
                    .unwrap_or("".to_string())
            })
            .unwrap_or("<collection />".to_string());

        self
    }

    pub fn build(self) -> String {
        let content_length_str = self.get_content_length.unwrap_or_default();
        let etag = self.get_etag.unwrap_or_default();

        format!(
            r#"<response>
        <href>{}</href>
        <propstat>
            <prop>
            <resourcetype>{}</resourcetype>
            <creationdate>{}</creationdate>
            <getcontentlength>{}</getcontentlength>
            <getlastmodified>{}</getlastmodified>
            <getetag>{}</getetag>
            <supportedlock>
                    <lockentry>
                        <lockscope>
                            <exclusive/>
                        </lockscope>
                        <locktype>
                            <write/>
                        </locktype>
                    </lockentry>
                    <lockentry>
                        <lockscope>
                            <shared/>
                        </lockscope>
                        <locktype>
                            <write/>
                        </locktype>
                    </lockentry>
                </supportedlock>
                <lockdiscovery/>
            <getcontenttype>{}</getcontenttype>
            </prop>
            <status>HTTP/1.1 200 OK</status>
        </propstat>
    </response>"#,
            self.href,
            self.resource_type,
            self.get_content_type,
            content_length_str,
            self.get_last_modified,
            etag,
            self.get_content_type,
        )
    }
}
