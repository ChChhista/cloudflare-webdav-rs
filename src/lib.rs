use base64::{engine::general_purpose, Engine as _};
use http::Method;
use std::{collections::HashMap, future::Future, pin::Pin};
use worker::*;

use crate::constant::*;
use crate::dav::DavBuilder;
mod constant;
mod dav;

/// [DAV header RFC](http://www.webdav.org/specs/rfc4918.html#HEADER_DAV)

#[event(fetch)]
async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let (username, password) = (
        env.var("USERNAME")?.to_string(),
        env.var("PASSWORD")?.to_string(),
    );
    let b64 = general_purpose::STANDARD.encode(format!("{}:{}", username, password));
    let bucket = env.bucket("bucket")?;

    // Ensure to return Ok, even if the http header is not set
    match req.headers().get("Authorization") {
        Ok(Some(auth)) if auth == format!("Basic {}", b64) => {
            let origin = req.headers().get("Origin")?.or(Some(String::from("*")));
            dispatch_request(req, bucket)
                .await?
                .with_cors(&set_cors_headers(origin))
        }
        _ => {
            let mut headers = Headers::new();
            headers.append("WWW-Authenticate", "Basic realm=\"webdav\"")?;
            Ok(Response::error("Unauthorized", 401)?.with_headers(headers))
        }
    }
}

async fn list_all_files(bucket: &Bucket, prefix: impl Into<String> + Copy) -> Result<Vec<Object>> {
    let mut files = vec![];
    let mut cursor = None;
    loop {
        let mut list_req = bucket
            .list()
            .include(vec![Include::HttpMetadata, Include::CustomMetadata]);
        if !prefix.into().is_empty() {
            list_req = list_req.prefix(prefix);
        }
        if let Some(c) = cursor {
            list_req = list_req.cursor(c);
        }
        let objects = list_req.execute().await?;
        files.extend(objects.objects());
        if !objects.truncated() {
            break;
        }
        cursor = objects.cursor();
    }
    Ok(files)
}

/// [Not advertised in OPTIONS response](http://www.webdav.org/specs/rfc4918.html#HEADER_DAV)
async fn handle_options(_req: Request, _bucket: Bucket) -> Result<Response> {
    let mut headers = Headers::new();
    headers.append("DAV", "1, 2")?;
    headers.append("Allow", METHODS.join(", ").as_str())?;
    Ok(Response::empty()?.with_status(204).with_headers(headers))
}

/// [HEAD method](http://www.webdav.org/specs/rfc4918.html#n-get--head-for-collections)
async fn handle_head(req: Request, bucket: Bucket) -> Result<Response> {
    let res = handle_get(req, bucket).await?;
    Ok(Response::empty()?
        .with_status(res.status_code())
        .with_headers(res.headers().clone()))
}

async fn handle_get(req: Request, bucket: Bucket) -> Result<Response> {
    let url = req.url()?;
    let key = url.path().trim_matches('/');
    if url.path().ends_with('/') {
        let page = r#"<!DOCTYPE HTML PUBLIC "-//IETF//DTD HTML 2.0//EN"><html><head><title>404 Not Found</title></head><body><h1>Not Found</h1><p>The requested URL was not found on this server.</p></body></html>"#;
        let mut headers = Headers::new();
        headers.append("Content-Type", "text/html")?;
        return Ok(Response::ok(page)?.with_headers(headers));
    }

    if req.headers().get("Range")?.is_none() {
        let object = bucket.get(key).execute().await?.ok_or("Object is None")?;
        let http_meta_data = object.http_metadata();
        let stream = object.body().ok_or("Body is None")?.stream()?;
        return Ok(Response::from_stream(stream)?.with_headers(get_headers(http_meta_data)?));
    }
    Response::error("Method Not allowed", 405)
}

async fn handle_delete(req: Request, bucket: Bucket) -> Result<Response> {
    let url = req.url()?;
    let key = url.path().trim_matches('/');

    let source = bucket.head(key).await?;
    if source.is_none() {
        let files = list_all_files(&bucket, key).await?;
        if files.is_empty() {
            return Response::error("Not Found", 404);
        }
        for f in files {
            bucket.delete(f.key()).await?;
        }
    }
    bucket.delete(key).await?;
    Ok(Response::empty()?.with_status(204))
}

async fn handle_proppatch(req: Request, bucket: Bucket) -> Result<Response> {
    todo!()
}

async fn handle_mkcol(req: Request, bucket: Bucket) -> Result<Response> {
    let url = req.url()?;
    let key = url.path().trim_matches('/');
    if key.is_empty() {
        return Response::error("Method Not Found", 405);
    }
    // flag: The folder has been created for R2.
    let flag = key.to_string() + "/";
    let object = bucket.head(&flag).await?;
    if object.is_some() {
        return Response::error("Conflict", 409);
    }
    bucket
        .put(flag, Data::from(String::from("")))
        .execute()
        .await?;
    Ok(Response::empty()?.with_status(201))
}

async fn handle_propfind(req: Request, bucket: Bucket) -> Result<Response> {
    let url = req.url()?;
    let key = url.path().trim_matches('/');
    let mut page = r#"<?xml version="1.0" encoding="utf-8"?>
<multistatus xmlns="DAV:">"#
        .to_string();

    let mut headers = Headers::new();
    headers.append("Content-Type", "text/xml")?;
    // R2 Storage lacks a traditional directory structure.
    if !url.path().ends_with('/') && !key.is_empty() {
        match bucket.head(key).await? {
            Some(object) => {
                let href = format!("/{}", object.key());
                page.push_str(&DavBuilder::new().object(&href, Some(&object)).build());
                page.push_str("</multistatus>");
                return Ok(Response::ok(page)?.with_headers(headers));
            }
            None => return Response::error("Not Found", 404),
        }
    }

    let href = format!("/{}", key);
    let mut xml = DavBuilder::new().object(&href, None).build();
    let depth = req.headers().get("Depth")?.unwrap_or(String::from("1"));

    match depth.as_str() {
        "0" => {
            xml.push_str("</multistatus>");
            page.push_str(&xml);
            Ok(Response::ok(page)?.with_headers(headers))
        }
        "1" => {
            let objects = list_all_files(&bucket, key).await?;
            if objects.is_empty() {
                return Response::error("Not Found", 404);
            }
            let mut keys = vec![key.to_string()];
            for object in objects {
                let mut o_key = &object.key()[key.len()..];
                o_key = o_key.trim_start_matches('/');
                if !o_key.contains('/') {
                    let href = format!("/{}", object.key());
                    xml.push_str(&DavBuilder::new().object(&href, Some(&object)).build());
                    continue;
                }
                // handle sub directory
                let folder_name = o_key.split('/').next().unwrap().to_string();
                if !keys.contains(&folder_name) {
                    keys.push(folder_name.clone());
                    let href = format!("/{}", folder_name);
                    xml.push_str(&DavBuilder::new().object(&href, None).build());
                }
            }
            xml.push_str("</multistatus>");
            page.push_str(&xml);

            Ok(Response::ok(page)?.with_headers(headers))
        }
        "infinity" => Response::error("Not Implemented", 501),
        _ => Response::error("Forbidden", 403),
    }
}

async fn handle_put(mut req: Request, bucket: Bucket) -> Result<Response> {
    let url = req.url()?;
    let key = url.path().trim_matches('/');
    if key.is_empty() {
        return Response::error("Method Not Found", 405);
    }
    let data = req.bytes().await?;
    bucket.put(key, Data::from(data)).execute().await?;
    Ok(Response::empty()?.with_status(201))
}

async fn handle_copy(req: Request, bucket: Bucket) -> Result<Response> {
    todo!()
}

async fn handle_move(req: Request, bucket: Bucket) -> Result<Response> {
    todo!()
}

async fn handle_lock(req: Request, _bucket: Bucket) -> Result<Response> {
    let depth = req.headers().get("Depth")?.unwrap_or(String::from("0"));
    let timeout = req
        .headers()
        .get("Timeout")?
        .unwrap_or(String::from("Infinite"));
    // TODO: parser xml and lock token
    // <D:locktoken>
    //   <D:href>opaquelocktoken:{}</D:href>
    // </D:locktoken>
    Response::ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<D:prop xmlns:D="DAV:">
  <D:lockdiscovery>
    <D:activelock>
      <D:locktype><D:write/></D:locktype>
      <D:lockscope><D:exclusive/></D:lockscope>
      <D:depth>{}</D:depth>
      <ns0:owner xmlns:ns0="DAV:">
        <ns0:href>http://www.apple.com/webdav_fs/</ns0:href>
      </ns0:owner>
        <D:timeout>{}</D:timeout>
    </D:activelock>
  </D:lockdiscovery>
</D:prop>"#,
        depth, timeout,
    ))
}

type AsyncHandler = Box<dyn Fn(Request, Bucket) -> Pin<Box<dyn Future<Output = Result<Response>>>>>;

async fn dispatch_request(req: Request, bucket: Bucket) -> Result<Response> {
    let mut handlers: HashMap<&str, AsyncHandler> = HashMap::new();
    handlers.insert(
        "GET",
        Box::new(|req, bucket| Box::pin(handle_get(req, bucket))),
    );
    handlers.insert(
        "DELETE",
        Box::new(|req, bucket| Box::pin(handle_delete(req, bucket))),
    );
    handlers.insert(
        "PROPPATCH",
        Box::new(|req, bucket| Box::pin(handle_proppatch(req, bucket))),
    );
    handlers.insert(
        "PUT",
        Box::new(|req, bucket| Box::pin(handle_put(req, bucket))),
    );
    handlers.insert(
        "HEAD",
        Box::new(|req, bucket| Box::pin(handle_head(req, bucket))),
    );
    handlers.insert(
        "OPTIONS",
        Box::new(|req, bucket| Box::pin(handle_options(req, bucket))),
    );
    handlers.insert(
        "MKCOL",
        Box::new(|req, bucket| Box::pin(handle_mkcol(req, bucket))),
    );
    handlers.insert(
        "PROPFIND",
        Box::new(|req, bucket| Box::pin(handle_propfind(req, bucket))),
    );
    handlers.insert(
        "COPY",
        Box::new(|req, bucket| Box::pin(handle_copy(req, bucket))),
    );
    handlers.insert(
        "MOVE",
        Box::new(|req, bucket| Box::pin(handle_move(req, bucket))),
    );
    handlers.insert(
        "LOCK",
        Box::new(|req, bucket| Box::pin(handle_lock(req, bucket))),
    );
    handlers.insert(
        "UNLOCK",
        Box::new(|_, _| Box::pin(async { Ok(Response::empty()?.with_status(204)) })),
    );

    match handlers.get(req.method().as_str()) {
        Some(handler) => handler(req, bucket).await,
        _ => Response::error("Method Not allowed", 405),
    }
}

fn set_cors_headers(origin: Option<String>) -> Cors {
    let methods = METHODS
        .iter()
        .map(|s| Method::from_bytes(s.as_bytes()).expect("[ERROR] Invalid method"))
        .collect::<Vec<Method>>();
    Cors::default()
        .with_origins(origin)
        .with_methods(methods)
        .with_allowed_headers(ALLOW_HEADERS)
        .with_credentials(false)
        .with_exposed_headers(EXPOSED_HEADERS)
        .with_max_age(86400)
        .with_credentials(false)
}

fn get_headers(meta: HttpMetadata) -> Result<Headers> {
    let mut headers = Headers::new();
    headers.append(
        "Content-Type",
        &meta
            .content_type
            .unwrap_or(String::from("application/octet-stream")),
    )?;
    if let Some(content_disposition) = meta.content_disposition {
        headers.append("Content-Disposition", &content_disposition)?;
    }
    if let Some(content_encoding) = meta.content_encoding {
        headers.append("Content-Encoding", &content_encoding)?;
    }
    if let Some(content_language) = meta.content_language {
        headers.append("Content-Language", &content_language)?;
    }
    if let Some(cache_control) = meta.cache_control {
        headers.append("Cache-Control", &cache_control)?;
    }
    if let Some(cache_expiry) = meta.cache_expiry {
        headers.append("Cache-Expires", &cache_expiry.to_string())?;
    }
    Ok(headers)
}
