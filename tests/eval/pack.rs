//! Committed fixtures: each recorded goal dir (`search.jsonl`,
//! `pages.jsonl`, `gateway.r<k>.jsonl`, `run.r<k>.json`) is stored
//! gzipped, one `.gz` per file, under `tests/eval_fixtures/<goal id>/`.
//! Pages are kept whole: grounding (`verification`) checks quotes against
//! them, so a trimmed page would change the metrics the offline gate
//! asserts.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

/// `tests/eval_fixtures` of this checkout.
pub fn committed_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eval_fixtures")
}

fn entries(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|err| format!("{}: {err}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_file())
        .collect();
    paths.sort();
    Ok(paths)
}

/// Gzip every `*.json`/`*.jsonl` of `from` into `to` (created; stale
/// files removed first).
pub fn pack(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|err| format!("{}: {err}", to.display()))?;
    for stale in entries(to)? {
        std::fs::remove_file(&stale).map_err(|err| format!("{}: {err}", stale.display()))?;
    }
    for path in entries(from)? {
        if path
            .extension()
            .is_none_or(|ext| ext != "jsonl" && ext != "json")
        {
            continue;
        }
        let raw = std::fs::read(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let name = path.file_name().expect("file has a name").to_string_lossy();
        let out = to.join(format!("{name}.gz"));
        let file =
            std::fs::File::create(&out).map_err(|err| format!("{}: {err}", out.display()))?;
        let mut encoder = GzEncoder::new(file, Compression::best());
        encoder
            .write_all(&raw)
            .and_then(|()| encoder.finish().map(drop))
            .map_err(|err| format!("{}: {err}", out.display()))?;
    }
    Ok(())
}

/// Gunzip every `*.gz` of `from` into `to` (created).
pub fn unpack(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|err| format!("{}: {err}", to.display()))?;
    for path in entries(from)? {
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".gz"))
        else {
            continue;
        };
        let file =
            std::fs::File::open(&path).map_err(|err| format!("{}: {err}", path.display()))?;
        let mut raw = Vec::new();
        GzDecoder::new(file)
            .read_to_end(&mut raw)
            .map_err(|err| format!("{}: {err}", path.display()))?;
        let out = to.join(name);
        std::fs::write(&out, raw).map_err(|err| format!("{}: {err}", out.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_then_unpack_round_trips_jsonl_files_only() {
        let base = std::env::temp_dir().join(format!("war-eval-pack-{}", std::process::id()));
        let (raw, packed, back) = (base.join("raw"), base.join("packed"), base.join("back"));
        std::fs::create_dir_all(&raw).unwrap();
        std::fs::write(raw.join("pages.jsonl"), "{\"a\":1}\n").unwrap();
        std::fs::write(raw.join("notes.txt"), "skip").unwrap();
        pack(&raw, &packed).expect("packs");
        assert!(packed.join("pages.jsonl.gz").is_file());
        assert!(!packed.join("notes.txt.gz").exists());
        unpack(&packed, &back).expect("unpacks");
        assert_eq!(
            std::fs::read_to_string(back.join("pages.jsonl")).unwrap(),
            "{\"a\":1}\n"
        );
        std::fs::remove_dir_all(&base).unwrap();
    }
}
