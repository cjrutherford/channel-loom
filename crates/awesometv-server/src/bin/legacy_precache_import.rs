//! Offline two-phase importer. Run with no arguments for usage.
#[path = "../precache/resume.rs"]
mod resume;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};
use std::{fs::{self, File, OpenOptions}, io::{Read, Write}, path::{Path, PathBuf}};

#[derive(Debug, Serialize, Deserialize)]
struct Marker {
    media: String, source: String, size: i64, mtime: i64, cache: String,
    status: String, prefix: i64, charge: i64, device: u64, inode: u64,
}
#[cfg(unix)]
fn inode(f: &File) -> Result<(u64,u64)> { use std::os::unix::fs::MetadataExt; let m=f.metadata()?; Ok((m.dev(),m.ino())) }

async fn row(pool: &SqlitePool, media: &str) -> Result<sqlx::sqlite::SqliteRow> {
    Ok(sqlx::query("SELECT media_id,source_path,source_size,source_mtime,cache_path,status,bytes,mode FROM precache_entries WHERE media_id=?")
        .bind(media).fetch_one(pool).await?)
}
fn partial(cache: &str) -> PathBuf { PathBuf::from(format!("{cache}.part")) }
fn manifest(p: &Path) -> PathBuf { PathBuf::from(format!("{}.resume",p.display())) }
fn sync_dir(p: &Path) -> Result<()> { File::open(p)?.sync_all()?; Ok(()) }

async fn prepare(pool: &SqlitePool, media: &str, root: &Path, protected: &Path) -> Result<()> {
    let root = fs::canonicalize(root)?;
    let parent = fs::canonicalize(protected.parent().context("protected directory needs a parent")?)?;
    ensure!(!parent.starts_with(&root), "protection must be outside cache root");
    let r=row(pool,media).await?;
    let cache: String=r.try_get("cache_path")?;
    ensure!(fs::canonicalize(Path::new(&cache).parent().context("cache parent")?)? == root, "cache path outside root");
    let status: String=r.get("status");
    let mode: String=r.get("mode");
    let size: i64=r.get("source_size"); let prefix: i64=r.get("bytes");
    ensure!(mode == "copy" && matches!(status.as_str(),"running"|"queued") && size > 0 && prefix > 0 && prefix <= size, "ineligible legacy job");
    let src=partial(&cache); let f=File::open(&src)?;
    f.sync_all()?;
    ensure!(f.metadata()?.len() >= prefix as u64, "recorded prefix exceeds durable file");
    let (device,inode_number)=inode(&f)?;
    // Directory creation is the exclusive marker; an interrupted prepare never replaces it.
    fs::create_dir(protected)?;
    fs::hard_link(&src,protected.join("protected.part"))?;
    ensure!(inode(&File::open(protected.join("protected.part"))?)? == (device,inode_number), "partial changed during prepare");
    let m=Marker { media:media.into(), source:r.get("source_path"),size,mtime:r.get("source_mtime"),cache,status,prefix,charge:size,device,inode:inode_number };
    let mut out=OpenOptions::new().write(true).create_new(true).open(protected.join("marker.tmp"))?;
    serde_json::to_writer(&mut out,&m)?; out.write_all(b"\n")?; out.sync_all()?;
    fs::rename(protected.join("marker.tmp"),protected.join("marker.json"))?;
    sync_dir(protected)?; sync_dir(&parent)?; Ok(())
}

async fn import(pool: &SqlitePool, protected: &Path, generation: &str, offline: bool) -> Result<()> {
    ensure!(offline, "--offline is required: all old and new workers must be stopped");
    ensure!(!generation.is_empty(), "empty generation");
    let m:Marker=serde_json::from_reader(File::open(protected.join("marker.json"))?)?;
    ensure!(m.size > 0 && m.prefix > 0 && m.prefix <= m.size && m.charge == m.size, "invalid marker");
    let backup=File::open(protected.join("protected.part"))?;
    ensure!(inode(&backup)? == (m.device,m.inode) && backup.metadata()?.len() >= m.prefix as u64, "protected inode or length changed");
    let dest=partial(&m.cache); let checkpoint=manifest(&dest);
    let identity=resume::Identity { path:m.source.clone(),size:m.size as u64,mtime:m.mtime };
    // Snapshot and CAS include every ownership/identity field; no remote I/O in the transaction.
    let current=sqlx::query("SELECT generation,status,source_path,source_size,source_mtime,cache_path,mode,identity_known,bytes FROM precache_entries WHERE media_id=?")
        .bind(&m.media).fetch_one(pool).await?;
    let status:String=current.get("status");
    ensure!(current.get::<String,_>("generation")==generation && matches!(status.as_str(),"running"|"queued") && current.get::<String,_>("source_path")==m.source && current.get::<i64,_>("source_size")==m.size && current.get::<i64,_>("source_mtime")==m.mtime && current.get::<Option<String>,_>("cache_path").as_deref()==Some(&m.cache) && current.get::<String,_>("mode")=="copy" && current.get::<i64,_>("identity_known")==1, "job identity or generation conflict");
    // The staging file stays in the protected directory for crash-safe ownership proof.
    let stage=protected.join("import.part");
    if stage.exists() {
        let a=File::open(&stage)?;
        if dest.exists() && inode(&a)? == inode(&File::open(&dest)?)? && checkpoint.exists() && status == "queued" && current.get::<i64,_>("bytes")==m.prefix {
            ensure!(a.metadata()?.len() == m.prefix as u64, "existing partial has advanced; refusing to modify it");
            ensure!(resume::load(&checkpoint,&dest,&m.media,generation,&identity)? == m.prefix as u64, "existing checkpoint mismatch");
            return Ok(());
        }
        bail!("previous incomplete import requires inspection");
    }
    ensure!(!dest.exists() && !checkpoint.exists() && !manifest(&dest).with_extension("resume.tmp").exists(), "destination already exists");
    let mut out=OpenOptions::new().write(true).create_new(true).open(&stage)?;
    let mut input=backup.take(m.prefix as u64);
    ensure!(std::io::copy(&mut input,&mut out)? == m.prefix as u64, "backup shortened"); out.sync_all()?; sync_dir(protected)?;
    let mut tx=pool.begin().await?;
    let result=sqlx::query("UPDATE precache_entries SET status='queued',bytes=?,error=NULL WHERE media_id=? AND generation=? AND status=? AND source_path=? AND source_size=? AND source_mtime=? AND cache_path=? AND mode='copy' AND identity_known=1")
        .bind(m.prefix).bind(&m.media).bind(generation).bind(&status).bind(&m.source).bind(m.size).bind(m.mtime).bind(&m.cache).execute(&mut *tx).await?;
    ensure!(result.rows_affected()==1, "job changed during import");
    // Exclusive links/placeholders prevent replacing a foreign partial or checkpoint.
    fs::hard_link(&stage,&dest)?;
    let placeholder=OpenOptions::new().write(true).create_new(true).open(&checkpoint);
    if let Err(e)=placeholder { let _=fs::remove_file(&dest); return Err(e.into()); }
    if let Err(e)=resume::save(&checkpoint,&dest,&m.media,generation,&identity,m.prefix as u64) {
        let _=fs::remove_file(&checkpoint); let _=fs::remove_file(&dest); return Err(e);
    }
    tx.commit().await?; Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let a:Vec<String>=std::env::args().collect();
    if a.len()!=6 && a.len()!=7 { bail!("usage: legacy_precache_import prepare DB MEDIA CACHE_ROOT PROTECTED_DIR | import DB PROTECTED_DIR GENERATION --offline"); }
    let opts=sqlx::sqlite::SqliteConnectOptions::new().filename(&a[2]).create_if_missing(false);
    let pool=sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await?;
    match a[1].as_str() {
        "prepare" if a.len()==6 => prepare(&pool,&a[3],Path::new(&a[4]),Path::new(&a[5])).await,
        "import" if a.len()==6 => import(&pool,Path::new(&a[3]),&a[4],a[5]=="--offline").await,
        _ => bail!("invalid arguments"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn setup() -> (PathBuf, SqlitePool, PathBuf, PathBuf) {
        let root=std::env::temp_dir().join(format!("legacy-import-{}",uuid::Uuid::new_v4()));
        let cache=root.join("cache"); fs::create_dir_all(&cache).unwrap();
        let protected=root.join("safe"); let dest=cache.join("item");
        let pool=SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE precache_entries(media_id TEXT PRIMARY KEY,source_path TEXT,source_size INTEGER,source_mtime INTEGER,cache_path TEXT,status TEXT,bytes INTEGER,mode TEXT,generation TEXT,identity_known INTEGER,error TEXT)").execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO precache_entries VALUES('item','/media/item',10,12,?,'running',4,'copy','new-generation',1,NULL)").bind(dest.to_str().unwrap()).execute(&pool).await.unwrap();
        fs::write(partial(dest.to_str().unwrap()),b"abcd").unwrap();
        (root,pool,protected,dest)
    }
    #[tokio::test]
    async fn append_unlink_then_resume_and_repeat() {
        let (root,pool,safe,dest)=setup().await;
        let part=partial(dest.to_str().unwrap());
        let mut writer=OpenOptions::new().append(true).open(&part).unwrap();
        prepare(&pool,"item",&root.join("cache"),&safe).await.unwrap();
        writer.write_all(b"efgh").unwrap(); writer.sync_all().unwrap(); fs::remove_file(&part).unwrap(); drop(writer);
        assert!(import(&pool,&safe,"new-generation",false).await.is_err());
        import(&pool,&safe,"new-generation",true).await.unwrap();
        import(&pool,&safe,"new-generation",true).await.unwrap();
        assert_eq!(fs::read(&part).unwrap(),b"abcd");
        assert_eq!(fs::read(safe.join("protected.part")).unwrap(),b"abcdefgh");
        let mut input=File::open({let p=root.join("source"); fs::write(&p,b"abcdefghij").unwrap(); p}).unwrap();
        let mut output=OpenOptions::new().read(true).write(true).open(&part).unwrap();
        assert_eq!(resume::copy_slice(&mut input,&mut output,4,10,6).unwrap(),10);
        assert_eq!(fs::read(&part).unwrap(),b"abcdefghij");
        assert!(import(&pool,&safe,"new-generation",true).await.is_err());
        assert_eq!(fs::read(&part).unwrap(),b"abcdefghij");
        fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn changed_generation_or_identity_never_exposes_partial() {
        let (root,pool,safe,dest)=setup().await;
        let part=partial(dest.to_str().unwrap());
        prepare(&pool,"item",&root.join("cache"),&safe).await.unwrap(); fs::remove_file(&part).unwrap();
        assert!(import(&pool,&safe,"wrong-generation",true).await.is_err());
        sqlx::query("UPDATE precache_entries SET source_mtime=99").execute(&pool).await.unwrap();
        assert!(import(&pool,&safe,"new-generation",true).await.is_err());
        assert!(!part.exists()); assert!(!manifest(&part).exists()); assert_eq!(fs::read(safe.join("protected.part")).unwrap(),b"abcd");
        fs::remove_dir_all(root).unwrap();
    }
}
