// CE-M0-2: 数据库后端集成测试。
// SQLite 用例默认运行;Postgres 用例打上 `#[ignore]` 标记,只有在
// 显式设置 `RUSTDESK_TEST_PG_URL` 且通过 `cargo test -- --ignored`
// 才会真正连接,避免 macOS 开发箱因缺少本地 PG 服务而被阻塞。

use hbb_common::tokio;
use hbbs::database::{mask_db_url, Database};

fn tmp_sqlite_path(tag: &str) -> String {
    let mut p = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    p.push(format!("rustdesk_ce_m0_2_it_{tag}_{nonce}.sqlite3"));
    p.to_string_lossy().to_string()
}

#[tokio::test]
async fn sqlite_legacy_default_path() {
    // 模拟旧部署:DSN 是裸路径,不带 scheme。
    let path = tmp_sqlite_path("legacy");
    let _ = std::fs::remove_file(&path);

    let db = Database::new(&path).await.expect("open ok");
    assert_eq!(db.backend_kind(), "sqlite");
    assert!(std::path::Path::new(&path).exists(), "文件应被自动创建");

    // 端到端 CRUD 一遍,等价于 peer.rs 中三处调用。
    let guid = db
        .insert_peer("legacy-alice", b"u", b"k", "{}")
        .await
        .unwrap();
    let got = db.get_peer("legacy-alice").await.unwrap().expect("hit");
    assert_eq!(got.pk, b"k".to_vec());
    db.update_pk(&guid, "legacy-alice", b"k2", "{\"ip\":\"1.1.1.1\"}")
        .await
        .unwrap();
    let got2 = db.get_peer("legacy-alice").await.unwrap().expect("hit");
    assert_eq!(got2.pk, b"k2".to_vec());

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn sqlite_max_connections_env_respected() {
    // 仅校验默认值与显式值都能完成 CRUD,不直接窥探池容量(deadpool 不暴露容量 getter)。
    let path = tmp_sqlite_path("maxconn");
    let _ = std::fs::remove_file(&path);

    // 显式给一个 != 1 的值,确保解析路径不破坏。
    std::env::set_var("MAX_DATABASE_CONNECTIONS", "4");
    let db = Database::new(&path).await.expect("open ok");
    // 简单 CRUD 一遍。
    let _ = db.insert_peer("maxc", b"u", b"k", "{}").await.unwrap();
    assert!(db.get_peer("maxc").await.unwrap().is_some());
    std::env::remove_var("MAX_DATABASE_CONNECTIONS");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn mask_db_url_basic() {
    assert_eq!(
        mask_db_url("postgres://alice:secret@127.0.0.1:5432/rd"),
        "postgres://alice:***@127.0.0.1:5432/rd"
    );
    assert_eq!(mask_db_url("./db_v2.sqlite3"), "./db_v2.sqlite3");
}

// ----- Postgres ignored tests -----

fn pg_url() -> Option<String> {
    std::env::var("RUSTDESK_TEST_PG_URL").ok()
}

#[tokio::test]
#[ignore]
async fn postgres_crud() {
    let url = match pg_url() {
        Some(u) => u,
        None => {
            eprintln!("RUSTDESK_TEST_PG_URL 未设置,跳过");
            return;
        }
    };

    let db = Database::new(&url).await.expect("connect ok");
    assert_eq!(db.backend_kind(), "postgres");

    // 用唯一 id 隔离生产 schema 上的潜在残留。
    let unique = format!(
        "rd_ce_m0_2_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    );

    let guid = db
        .insert_peer(&unique, b"u", b"k", "{}")
        .await
        .expect("insert ok");
    let got = db.get_peer(&unique).await.unwrap().expect("hit");
    assert_eq!(got.pk, b"k".to_vec());
    assert_eq!(got.guid, guid);

    db.update_pk(&guid, &unique, b"k2", "{\"ip\":\"2.2.2.2\"}")
        .await
        .unwrap();
    let got2 = db.get_peer(&unique).await.unwrap().expect("hit");
    assert_eq!(got2.pk, b"k2".to_vec());
    assert_eq!(got2.info, "{\"ip\":\"2.2.2.2\"}");
}

#[tokio::test]
#[ignore]
async fn postgres_create_tables_idempotent() {
    let url = match pg_url() {
        Some(u) => u,
        None => {
            eprintln!("RUSTDESK_TEST_PG_URL 未设置,跳过");
            return;
        }
    };
    // 连续两次 Database::new 不应报错(create table if not exists + create index if not exists 都幂等)。
    let _db1 = Database::new(&url).await.expect("first ok");
    let _db2 = Database::new(&url).await.expect("second ok");
}
