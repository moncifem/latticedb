//! Compatibility harness compiled against immutable pre-LCIX source, not a copied decoder.
use lattice_core::Config;
use lattice_engines::store::RowStore;
use lattice_seams::{OsRng, StdIo, SystemClock};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).ok_or("missing fixture root")?;
    let mode = std::env::args().nth(2).ok_or("missing expectation")?;
    let result = RowStore::open(StdIo::new(root), SystemClock, OsRng, &Config::default());
    match (mode.as_str(), result) {
        ("legacy" | "legacy-any", Ok(store)) => {
            let value = store
                .get_at(store.snapshot(), 7, b"a")
                .map_err(|error| error.to_string())?;
            let require_row =
                mode == "legacy" || std::env::args().nth(3).as_deref() == Some("row-required");
            if require_row && value.map(Vec::as_slice) != Some(b"x".as_slice()) {
                return Err("legacy literal row did not survive".into());
            }
            println!("legacy store accepted; literal_row_required={require_row}");
        }
        ("guarded", Err(error)) => {
            let message = error.to_string();
            if !message.contains("header") || !message.contains("BadMagic") {
                return Err(format!("unrelated refusal: {message}").into());
            }
            println!("guarded header refused: {message}");
        }
        _ => return Err("old-reader admission differs from fixture expectation".into()),
    }
    Ok(())
}
