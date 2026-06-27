use async_trait::async_trait;
use hbb_common::{log, ResultType};
use sqlx::{
    postgres::{PgPool, PgPoolOptions, PgRow},
    sqlite::{SqliteConnectOptions, SqliteRow},
    ConnectOptions, Connection, Error as SqlxError, Row, SqliteConnection,
};
use std::{ops::DerefMut, str::FromStr};

// CE-M0-2: SQLite / PostgreSQL 双后端抽象。
// SQLite 走原有 deadpool 池(默认 max=1,保持老部署字节级行为);
// Postgres 走 sqlx 内置 PgPool(默认 max=32)。
// 选择策略见 select_backend()。

type SqlitePool = deadpool::managed::Pool<DbPool>;

pub struct DbPool {
    url: String,
}

#[async_trait]
impl deadpool::managed::Manager for DbPool {
    type Type = SqliteConnection;
    type Error = SqlxError;
    async fn create(&self) -> Result<SqliteConnection, SqlxError> {
        let mut opt = SqliteConnectOptions::from_str(&self.url).unwrap();
        opt.log_statements(log::LevelFilter::Debug);
        SqliteConnection::connect_with(&opt).await
    }
    async fn recycle(
        &self,
        obj: &mut SqliteConnection,
    ) -> deadpool::managed::RecycleResult<SqlxError> {
        Ok(obj.ping().await?)
    }
}

#[derive(Clone)]
pub struct Database {
    inner: Backend,
}

#[derive(Clone)]
enum Backend {
    Sqlite(SqlitePool),
    Postgres(PgPool),
}

#[derive(Default)]
pub struct Peer {
    pub guid: Vec<u8>,
    pub id: String,
    pub uuid: Vec<u8>,
    pub pk: Vec<u8>,
    pub user: Option<Vec<u8>>,
    pub info: String,
    pub status: Option<i64>,
}

// DSN scheme 解析:postgres://, postgresql:// → Postgres;sqlite://、裸路径 → SQLite。
enum BackendKind {
    Sqlite,
    Postgres,
}

fn select_backend(url: &str) -> BackendKind {
    if url.starts_with("postgres://") || url.starts_with("postgresql://") {
        BackendKind::Postgres
    } else {
        BackendKind::Sqlite
    }
}

// SQLite 后端剥离可选的 `sqlite://` 前缀,得到落盘文件路径。
fn sqlite_file_path(url: &str) -> &str {
    url.strip_prefix("sqlite://").unwrap_or(url)
}

impl Database {
    pub async fn new(url: &str) -> ResultType<Database> {
        match select_backend(url) {
            BackendKind::Sqlite => Self::new_sqlite(url).await,
            BackendKind::Postgres => Self::new_postgres(url).await,
        }
    }

    async fn new_sqlite(url: &str) -> ResultType<Database> {
        let path = sqlite_file_path(url);
        if !std::path::Path::new(path).exists() {
            std::fs::File::create(path).ok();
        }
        let n: usize = std::env::var("MAX_DATABASE_CONNECTIONS")
            .unwrap_or_else(|_| "1".to_owned())
            .parse()
            .unwrap_or(1);
        log::debug!("MAX_DATABASE_CONNECTIONS={}", n);
        let pool = SqlitePool::new(
            DbPool {
                url: path.to_owned(),
            },
            n,
        );
        let _ = pool.get().await?; // test
        let db = Database {
            inner: Backend::Sqlite(pool),
        };
        db.create_tables().await?;
        Ok(db)
    }

    async fn new_postgres(url: &str) -> ResultType<Database> {
        let n: u32 = std::env::var("MAX_DATABASE_CONNECTIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(32);
        log::debug!("MAX_DATABASE_CONNECTIONS={}", n);
        let pool = PgPoolOptions::new().max_connections(n).connect(url).await?;
        let db = Database {
            inner: Backend::Postgres(pool),
        };
        db.create_tables().await?;
        Ok(db)
    }

    pub fn backend_kind(&self) -> &'static str {
        match self.inner {
            Backend::Sqlite(_) => "sqlite",
            Backend::Postgres(_) => "postgres",
        }
    }

    async fn create_tables(&self) -> ResultType<()> {
        match &self.inner {
            Backend::Sqlite(pool) => {
                // SQLite 方言原样保留,与历史 commit 字节级一致。
                sqlx::query(
                    "
            create table if not exists peer (
                guid blob primary key not null,
                id varchar(100) not null,
                uuid blob not null,
                pk blob not null,
                created_at datetime not null default(current_timestamp),
                user blob,
                status tinyint,
                note varchar(300),
                info text not null
            ) without rowid;
            create unique index if not exists index_peer_id on peer (id);
            create index if not exists index_peer_user on peer (user);
            create index if not exists index_peer_created_at on peer (created_at);
            create index if not exists index_peer_status on peer (status);
        ",
                )
                .execute(pool.get().await?.deref_mut())
                .await?;
            }
            Backend::Postgres(pool) => {
                // PG 默认 simple-query 不允许一条 prepare 带多条语句,逐条执行最稳。
                // `user` 是 PG 保留字,必须加双引号;`tinyint` → `smallint`;`blob` → `bytea`。
                let stmts = [
                    "create table if not exists peer (
                        guid          bytea       primary key not null,
                        id            varchar(100) not null,
                        uuid          bytea       not null,
                        pk            bytea       not null,
                        created_at    timestamptz not null default current_timestamp,
                        \"user\"      bytea,
                        status        smallint,
                        note          varchar(300),
                        info          text        not null
                    )",
                    "create unique index if not exists index_peer_id on peer (id)",
                    "create index if not exists index_peer_user on peer (\"user\")",
                    "create index if not exists index_peer_created_at on peer (created_at)",
                    "create index if not exists index_peer_status on peer (status)",
                ];
                for s in stmts {
                    sqlx::query(s).execute(pool).await?;
                }
            }
        }
        Ok(())
    }

    pub async fn get_peer(&self, id: &str) -> ResultType<Option<Peer>> {
        match &self.inner {
            Backend::Sqlite(pool) => {
                let row: Option<SqliteRow> = sqlx::query(
                    "select guid, id, uuid, pk, user, status, info from peer where id = ?",
                )
                .bind(id)
                .fetch_optional(pool.get().await?.deref_mut())
                .await?;
                Ok(row.map(|r| Peer {
                    guid: r.try_get("guid").unwrap_or_default(),
                    id: r.try_get("id").unwrap_or_default(),
                    uuid: r.try_get("uuid").unwrap_or_default(),
                    pk: r.try_get("pk").unwrap_or_default(),
                    user: r.try_get("user").ok(),
                    status: r.try_get("status").ok(),
                    info: r.try_get("info").unwrap_or_default(),
                }))
            }
            Backend::Postgres(pool) => {
                let row: Option<PgRow> = sqlx::query(
                    "select guid, id, uuid, pk, \"user\", status, info from peer where id = $1",
                )
                .bind(id)
                .fetch_optional(pool)
                .await?;
                Ok(row.map(|r| {
                    // PG status 列类型为 smallint → i16,统一向上转换到 i64,保持外部契约一致。
                    let status: Option<i16> = r.try_get("status").ok();
                    Peer {
                        guid: r.try_get("guid").unwrap_or_default(),
                        id: r.try_get("id").unwrap_or_default(),
                        uuid: r.try_get("uuid").unwrap_or_default(),
                        pk: r.try_get("pk").unwrap_or_default(),
                        user: r.try_get("user").ok(),
                        status: status.map(|v| v as i64),
                        info: r.try_get("info").unwrap_or_default(),
                    }
                }))
            }
        }
    }

    pub async fn insert_peer(
        &self,
        id: &str,
        uuid: &[u8],
        pk: &[u8],
        info: &str,
    ) -> ResultType<Vec<u8>> {
        let guid = uuid::Uuid::new_v4().as_bytes().to_vec();
        match &self.inner {
            Backend::Sqlite(pool) => {
                sqlx::query("insert into peer(guid, id, uuid, pk, info) values(?, ?, ?, ?, ?)")
                    .bind(&guid)
                    .bind(id)
                    .bind(uuid)
                    .bind(pk)
                    .bind(info)
                    .execute(pool.get().await?.deref_mut())
                    .await?;
            }
            Backend::Postgres(pool) => {
                sqlx::query(
                    "insert into peer(guid, id, uuid, pk, info) values($1, $2, $3, $4, $5)",
                )
                .bind(&guid)
                .bind(id)
                .bind(uuid)
                .bind(pk)
                .bind(info)
                .execute(pool)
                .await?;
            }
        }
        Ok(guid)
    }

    pub async fn update_pk(
        &self,
        guid: &Vec<u8>,
        id: &str,
        pk: &[u8],
        info: &str,
    ) -> ResultType<()> {
        match &self.inner {
            Backend::Sqlite(pool) => {
                sqlx::query("update peer set id=?, pk=?, info=? where guid=?")
                    .bind(id)
                    .bind(pk)
                    .bind(info)
                    .bind(guid)
                    .execute(pool.get().await?.deref_mut())
                    .await?;
            }
            Backend::Postgres(pool) => {
                sqlx::query("update peer set id=$1, pk=$2, info=$3 where guid=$4")
                    .bind(id)
                    .bind(pk)
                    .bind(info)
                    .bind(guid)
                    .execute(pool)
                    .await?;
            }
        }
        Ok(())
    }
}

// 对 DSN 字符串中的密码做脱敏(用于日志打印),避免敏感信息落盘。
// 仅匹配标准 URL 格式 `<scheme>://<user>:<password>@host`,匹配不到则原样返回。
pub fn mask_db_url(url: &str) -> String {
    if let Some(idx_scheme) = url.find("://") {
        let (scheme, rest) = url.split_at(idx_scheme + 3);
        if let Some(at) = rest.find('@') {
            let userinfo = &rest[..at];
            let tail = &rest[at..];
            if let Some(colon) = userinfo.find(':') {
                let user = &userinfo[..colon];
                return format!("{scheme}{user}:***{tail}");
            }
        }
    }
    url.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use hbb_common::tokio;

    // 原 10k 并发压测,显式 `#[ignore]`,默认不跑;`cargo test -- --ignored` 触发。
    #[test]
    #[ignore]
    fn test_insert() {
        insert();
    }

    #[tokio::main(flavor = "multi_thread")]
    async fn insert() {
        let db = super::Database::new("test.sqlite3").await.unwrap();
        let mut jobs = vec![];
        for i in 0..10000 {
            let cloned = db.clone();
            let id = i.to_string();
            let a = tokio::spawn(async move {
                let empty_vec = Vec::new();
                cloned
                    .insert_peer(&id, &empty_vec, &empty_vec, "")
                    .await
                    .unwrap();
            });
            jobs.push(a);
        }
        for i in 0..10000 {
            let cloned = db.clone();
            let id = i.to_string();
            let a = tokio::spawn(async move {
                cloned.get_peer(&id).await.unwrap();
            });
            jobs.push(a);
        }
        hbb_common::futures::future::join_all(jobs).await;
    }

    fn tmp_sqlite_path(tag: &str) -> String {
        let mut p = std::env::temp_dir();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        p.push(format!("rustdesk_ce_m0_2_{tag}_{nonce}.sqlite3"));
        p.to_string_lossy().to_string()
    }

    #[tokio::test]
    async fn test_sqlite_crud() {
        let path = tmp_sqlite_path("crud");
        let _ = std::fs::remove_file(&path);
        let db = Database::new(&path).await.unwrap();
        assert_eq!(db.backend_kind(), "sqlite");

        let guid = db
            .insert_peer("alice", b"u", b"k", "{}")
            .await
            .expect("insert ok");
        let got = db.get_peer("alice").await.unwrap().expect("found");
        assert_eq!(got.id, "alice");
        assert_eq!(got.pk, b"k".to_vec());
        assert_eq!(got.guid, guid);
        assert_eq!(got.info, "{}");

        db.update_pk(&guid, "alice", b"k2", "{\"ip\":\"1.1.1.1\"}")
            .await
            .unwrap();
        let got2 = db.get_peer("alice").await.unwrap().expect("found");
        assert_eq!(got2.pk, b"k2".to_vec());
        assert_eq!(got2.info, "{\"ip\":\"1.1.1.1\"}");
        assert_eq!(got2.guid, guid, "guid 不应被改变");

        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_sqlite_missing_peer() {
        let path = tmp_sqlite_path("missing");
        let _ = std::fs::remove_file(&path);
        let db = Database::new(&path).await.unwrap();
        let got = db.get_peer("not-exist").await.unwrap();
        assert!(got.is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn test_sqlite_path_with_scheme() {
        let path = tmp_sqlite_path("scheme");
        let url = format!("sqlite://{path}");
        let _ = std::fs::remove_file(&path);
        let db = Database::new(&url).await.unwrap();
        assert_eq!(db.backend_kind(), "sqlite");
        // 应该创建剥离前缀后的文件,而不是名为 "sqlite:/..." 的怪文件。
        assert!(std::path::Path::new(&path).exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mask_db_url() {
        assert_eq!(
            mask_db_url("postgres://user:secret@host:5432/db"),
            "postgres://user:***@host:5432/db"
        );
        assert_eq!(
            mask_db_url("postgresql://u:p@h/d"),
            "postgresql://u:***@h/d"
        );
        // 无密码 / 无用户名时原样返回。
        assert_eq!(
            mask_db_url("sqlite://./db_v2.sqlite3"),
            "sqlite://./db_v2.sqlite3"
        );
        assert_eq!(mask_db_url("./db_v2.sqlite3"), "./db_v2.sqlite3");
    }
}
