//! Durable local checkpoints for resumable copies.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Identity {
    pub path: String,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u8,
    owner: String,
    #[serde(default)]
    generation: String,
    identity: Identity,
    partial_name: String,
    written: u64,
    reserved: u64,
}

fn filename(path: &Path) -> Result<String> {
    Ok(path
        .file_name()
        .and_then(|s| s.to_str())
        .context("invalid partial filename")?
        .to_owned())
}

// Caller must hold the exclusive job claim and reservation. Sync data before
// atomically replacing the manifest, so its recorded prefix survives a crash.
pub(super) fn save(
    manifest: &Path,
    partial: &Path,
    owner: &str,
    generation: &str,
    identity: &Identity,
    written: u64,
) -> Result<()> {
    if owner.is_empty()
        || generation.is_empty()
        || identity.size == 0
        || written > identity.size
        || manifest.parent() != partial.parent()
    {
        bail!("invalid checkpoint");
    }
    let file = OpenOptions::new().write(true).open(partial)?;
    if file.metadata()?.len() != written {
        bail!("partial length differs from checkpoint");
    }
    file.sync_all()?;
    let checkpoint = Checkpoint {
        version: 2,
        owner: owner.to_owned(),
        generation: generation.to_owned(),
        identity: identity.clone(),
        partial_name: filename(partial)?,
        written,
        reserved: identity.size,
    };
    let temporary = manifest.with_extension("resume.tmp");
    // Only the owner holding the claim may clean an abandoned temp manifest.
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut out, &checkpoint)?;
    out.sync_all()?;
    fs::rename(&temporary, manifest)?;
    File::open(manifest.parent().context("missing manifest directory")?)?.sync_all()?;
    Ok(())
}

// Never trust bytes beyond the durable checkpoint, even if they are present.
// A shorter local file is conservatively resumed from its actual length.
pub(super) fn load(
    manifest: &Path,
    partial: &Path,
    owner: &str,
    generation: &str,
    identity: &Identity,
) -> Result<u64> {
    if manifest.parent() != partial.parent() {
        bail!("partial outside checkpoint directory");
    }
    let checkpoint: Checkpoint = serde_json::from_reader(File::open(manifest)?)?;
    if checkpoint.version != 2
        || generation.is_empty()
        || checkpoint.generation != generation
        || checkpoint.owner != owner
        || checkpoint.identity != *identity
        || checkpoint.partial_name != filename(partial)?
        || checkpoint.reserved != identity.size
        || checkpoint.written > identity.size
    {
        bail!("checkpoint identity or ownership mismatch");
    }
    let file = OpenOptions::new().write(true).open(partial)?;
    let length = file.metadata()?.len();
    if length > identity.size {
        bail!("partial exceeds source size");
    }
    let verified = length.min(checkpoint.written);
    if length != verified {
        file.set_len(verified)?;
        file.sync_all()?;
    }
    Ok(verified)
}

// Copy a bounded prefix only after the caller has verified the source identity,
// acquired its conditional job claim, and retained the full-size reservation.
// The caller must recheck identity before publication and never expose this file.
#[allow(dead_code)]
pub(super) fn copy_slice(
    input: &mut File,
    output: &mut File,
    offset: u64,
    size: u64,
    quantum: u64,
) -> Result<u64> {
    if offset > size || quantum == 0 || output.metadata()?.len() != offset {
        bail!("invalid copy offset");
    }
    input.seek(SeekFrom::Start(offset))?;
    output.seek(SeekFrom::Start(offset))?;
    let mut limited = input.take(quantum.min(size - offset));
    let copied = std::io::copy(&mut limited, output)?;
    if copied != quantum.min(size - offset) {
        bail!("source ended before expected size");
    }
    Ok(offset + copied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn slices_resume_from_durable_prefix_after_reopen() {
        let root = std::env::temp_dir().join(format!(
            "atv-slices-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source");
        let partial = root.join("partial");
        let manifest = root.join("checkpoint.resume");
        let bytes: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&source, &bytes).unwrap();
        let identity = Identity {
            path: source.to_str().unwrap().into(),
            size: bytes.len() as u64,
            mtime: 1,
        };
        let mut input = File::open(&source).unwrap();
        let mut output = File::create(&partial).unwrap();
        let first = copy_slice(&mut input, &mut output, 0, identity.size, 16_000).unwrap();
        save(&manifest, &partial, "owner", "generation", &identity, first).unwrap();
        drop((input, output));
        let offset = load(&manifest, &partial, "owner", "generation", &identity).unwrap();
        let mut input = File::open(&source).unwrap();
        let mut output = OpenOptions::new().write(true).open(&partial).unwrap();
        let end = copy_slice(
            &mut input,
            &mut output,
            offset,
            identity.size,
            identity.size,
        )
        .unwrap();
        assert_eq!(end, identity.size);
        assert_eq!(std::fs::read(&partial).unwrap(), bytes);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn checkpoint_recovers_conservatively_and_rejects_changed_generation() {
        let root = std::env::temp_dir().join(format!(
            "atv-resume-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let partial = root.join("item.part");
        let manifest = root.join("item.resume");
        std::fs::write(&partial, b"abcdefgh").unwrap();
        let identity = Identity {
            path: "/source".into(),
            size: 12,
            mtime: 42,
        };
        save(&manifest, &partial, "item", "generation", &identity, 8).unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&partial)
            .unwrap()
            .set_len(5)
            .unwrap();
        assert_eq!(
            load(&manifest, &partial, "item", "generation", &identity).unwrap(),
            5
        );
        let changed = Identity {
            mtime: 43,
            ..identity.clone()
        };
        assert!(load(&manifest, &partial, "item", "generation", &changed).is_err());
        std::fs::OpenOptions::new()
            .append(true)
            .open(&partial)
            .unwrap()
            .write_all(b"0123456789")
            .unwrap();
        assert!(load(&manifest, &partial, "item", "generation", &identity).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
