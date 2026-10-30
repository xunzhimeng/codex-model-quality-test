use crate::accounts;
use gateway_plugin_sdk::client::HostClient;
use reqwest::{Client, Proxy, redirect::Policy};
use serde::Deserialize;
use std::time::Duration;

// 不实现Debug/Serialize，代理认证和令牌只存在于当前调用，不进入状态或诊断。
#[derive(Deserialize)]
struct Export {
    documents: Vec<Document>,
}
#[derive(Deserialize)]
struct Document {
    provider: String,
    document: Accounts,
}
#[derive(Deserialize)]
struct Accounts {
    accounts: Vec<Material>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Material {
    id: String,
    access_token: Option<String>,
    account_id: Option<String>,
    outbound_proxy_url: Option<String>,
}
pub struct Connection {
    pub client: Client,
    pub token: String,
    pub upstream_id: String,
    pub transport: &'static str,
}

pub async fn connection(
    host: &HostClient,
    id: &str,
    upstream_id: &str,
    proxy_required: bool,
) -> Result<Connection, String> {
    // 导出合同以逗号分割ID；只接受内置账号的安全标识，禁止编码后变成多账号选择。
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return Err("账号标识不适用于单账号导出".into());
    }
    let encoded = id;
    let mut export: Export = accounts::admin(
        host,
        format!(
            "/api/admin/accounts/export?accountIds={encoded}&confirm=export_sensitive_accounts"
        ),
        None,
    )
    .await?;
    if export.documents.len() != 1 {
        return Err("单账号导出数量不匹配，未发送探针".into());
    }
    let mut document = export.documents.pop().unwrap();
    if document.provider != "openai" || document.document.accounts.len() != 1 {
        return Err("账号导出来源不匹配，未发送探针".into());
    }
    let material = document.document.accounts.pop().unwrap();
    if material.id != id || material.account_id.as_deref() != Some(upstream_id) {
        return Err("账号导出ID不匹配，未发送探针".into());
    }
    let proxy = material.outbound_proxy_url.filter(|s| !s.trim().is_empty());
    // 脱敏目录声明了代理而敏感导出缺失时停止，不能把配置缺口解释成允许直连。
    if proxy_required && proxy.is_none() {
        return Err("账号代理配置已变化或导出缺少代理，请刷新后重试".into());
    }
    let token = material
        .access_token
        .filter(|s| !s.is_empty())
        .ok_or("账号导出缺少访问令牌")?;
    let upstream_id = material
        .account_id
        .filter(|s| !s.is_empty())
        .ok_or("账号导出缺少上游标识")?;
    Ok(Connection {
        client: client(proxy.as_deref())?,
        token,
        upstream_id,
        transport: if proxy.is_some() {
            "account_proxy"
        } else {
            "direct"
        },
    })
}

fn client(proxy: Option<&str>) -> Result<Client, String> {
    // 禁用环境代理；有账号代理时只走该代理，无配置时才使用服务器出口。
    let mut builder = Client::builder().no_proxy();
    if let Some(proxy) = proxy {
        let url = reqwest::Url::parse(proxy).map_err(|_| "账号代理格式无效")?;
        if !matches!(url.scheme(), "http" | "https" | "socks5" | "socks5h")
            || url.host_str().is_none()
            || url.port_or_known_default().is_none()
            || url.query().is_some()
            || url.fragment().is_some()
            || !matches!(url.path(), "" | "/")
        {
            return Err("账号代理格式无效".into());
        }
        builder = builder.proxy(Proxy::all(proxy).map_err(|_| "账号代理配置无效")?);
    }
    builder
        .redirect(Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(8))
        .timeout(Duration::from_secs(85))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| "探针网络客户端初始化失败".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        thread,
    };
    #[tokio::test]
    async fn http_proxy_is_used_and_failure_never_contacts_target() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let target_url = format!("http://{}/test", target.local_addr().unwrap());
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = proxy.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let worker = thread::spawn(move || {
            let (mut stream, _) = proxy.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = [0; 4096];
            let n = stream.read(&mut bytes).unwrap();
            let headers = String::from_utf8_lossy(&bytes[..n]);
            assert!(headers.starts_with("GET http://"));
            assert!(
                headers
                    .to_lowercase()
                    .contains("proxy-authorization: basic")
            );
            seen.fetch_add(1, Ordering::SeqCst);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        });
        let client = client(Some(&format!("http://user:password@{address}"))).unwrap();
        assert_eq!(
            client
                .get(&target_url)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "OK"
        );
        worker.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert!(target.accept().is_err());
        assert!(client.get(&target_url).send().await.is_err());
        assert!(target.accept().is_err());
    }
    #[tokio::test]
    async fn socks_proxy_failure_never_contacts_target() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let port = target.local_addr().unwrap().port();
        let proxy = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = proxy.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut s, _) = proxy.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut greeting = [0; 2];
            s.read_exact(&mut greeting).unwrap();
            assert_eq!(greeting[0], 5);
            let mut methods = vec![0; greeting[1] as usize];
            s.read_exact(&mut methods).unwrap();
            assert!(methods.contains(&2));
            s.write_all(&[5, 2]).unwrap();
            let mut header = [0; 2];
            s.read_exact(&mut header).unwrap();
            assert_eq!(header[0], 1);
            let mut username = vec![0; header[1] as usize];
            s.read_exact(&mut username).unwrap();
            assert_eq!(username, b"user");
            let mut len = [0; 1];
            s.read_exact(&mut len).unwrap();
            let mut password = vec![0; len[0] as usize];
            s.read_exact(&mut password).unwrap();
            assert_eq!(password, b"password");
            s.write_all(&[1, 0]).unwrap();
            let mut request = [0; 4];
            s.read_exact(&mut request).unwrap();
            assert_eq!(&request[..3], &[5, 1, 0]);
            assert_eq!(request[3], 3);
            s.read_exact(&mut len).unwrap();
            let mut name = vec![0; len[0] as usize];
            s.read_exact(&mut name).unwrap();
            assert_eq!(name, b"localhost");
            let mut target_port = [0; 2];
            s.read_exact(&mut target_port).unwrap();
            assert_eq!(u16::from_be_bytes(target_port), port);
            s.write_all(&[5, 5, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
        });
        let client = client(Some(&format!("socks5h://user:password@{address}"))).unwrap();
        assert!(
            client
                .get(format!("http://localhost:{port}/"))
                .send()
                .await
                .is_err()
        );
        worker.join().unwrap();
        assert!(target.accept().is_err());
    }
    #[tokio::test]
    async fn unconfigured_account_client_connects_directly() {
        let target = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = target.local_addr().unwrap();
        let worker = thread::spawn(move || {
            let (mut stream, _) = target.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = [0; 4096];
            let n = stream.read(&mut bytes).unwrap();
            let headers = String::from_utf8_lossy(&bytes[..n]);
            assert!(headers.starts_with("GET /direct HTTP/1.1"));
            assert!(!headers.to_lowercase().contains("proxy-authorization"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        });
        assert_eq!(
            client(None)
                .unwrap()
                .get(format!("http://{address}/direct"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "OK"
        );
        worker.join().unwrap();
    }
    #[test]
    fn rejects_invalid_configured_proxy_without_using_direct_mode() {
        assert!(client(Some("")).is_err());
        assert!(client(Some("file:///tmp/proxy")).is_err());
        assert!(client(Some("http://localhost:1/?secret=1")).is_err());
        assert!(client(None).is_ok());
    }
}
