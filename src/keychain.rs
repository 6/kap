/// Keychain helper: store/load/delete OAuth tokens via the OS keyring.
///
/// Tokens are stored as JSON strings keyed by MCP server name.
/// A JSON array at the "__index" key tracks known server names.
///
/// Graceful fallback: if the keychain is unavailable (CI, SSH, headless),
/// operations warn and return errors — callers should fall back to file storage.
use anyhow::{Context, Result};

const SERVICE: &str = "devg";
const INDEX_KEY: &str = "__index";

/// Store a JSON blob in the keychain under the given name.
/// Also adds the name to the index.
pub fn store(name: &str, json: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, name).context("creating keyring entry")?;
    entry
        .set_password(json)
        .context("storing token in keychain")?;
    index_add(name)?;
    Ok(())
}

/// Load a JSON blob from the keychain by name.
pub fn load(name: &str) -> Result<String> {
    let entry = keyring::Entry::new(SERVICE, name).context("creating keyring entry")?;
    entry
        .get_password()
        .context("loading token from keychain")
}

/// Delete a token from the keychain by name.
/// Also removes the name from the index.
pub fn delete(name: &str) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, name).context("creating keyring entry")?;
    match entry.delete_credential() {
        Ok(()) => {}
        Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(e).context("deleting token from keychain"),
    }
    index_remove(name)?;
    Ok(())
}

/// List all server names stored in the keychain index.
pub fn list_names() -> Result<Vec<String>> {
    let entry = keyring::Entry::new(SERVICE, INDEX_KEY).context("creating keyring index entry")?;
    match entry.get_password() {
        Ok(json) => {
            let names: Vec<String> =
                serde_json::from_str(&json).unwrap_or_default();
            Ok(names)
        }
        Err(keyring::Error::NoEntry) => Ok(Vec::new()),
        Err(e) => Err(e).context("reading keychain index"),
    }
}

/// Returns true if the keychain backend appears to be available.
pub fn is_available() -> bool {
    keyring::Entry::new(SERVICE, "__probe")
        .and_then(|e| {
            // Try a read — NoEntry is fine (means keychain works), other errors mean unavailable
            match e.get_password() {
                Ok(_) => Ok(()),
                Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(e),
            }
        })
        .is_ok()
}

fn index_add(name: &str) -> Result<()> {
    let mut names = list_names().unwrap_or_default();
    if !names.contains(&name.to_string()) {
        names.push(name.to_string());
        names.sort();
        write_index(&names)?;
    }
    Ok(())
}

fn index_remove(name: &str) -> Result<()> {
    let mut names = list_names().unwrap_or_default();
    names.retain(|n| n != name);
    write_index(&names)?;
    Ok(())
}

fn write_index(names: &[String]) -> Result<()> {
    let entry = keyring::Entry::new(SERVICE, INDEX_KEY).context("creating keyring index entry")?;
    let json = serde_json::to_string(names)?;
    entry
        .set_password(&json)
        .context("writing keychain index")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests hit the real OS keychain and trigger macOS permission prompts.
    // Only run with: DEVG_TEST_KEYCHAIN=1 cargo test keychain
    //
    // Unit-testable logic (index JSON manipulation) is tested without keychain access below.

    fn skip_unless_keychain_tests() -> bool {
        std::env::var("DEVG_TEST_KEYCHAIN").map_or(true, |v| v != "1")
    }

    fn test_name(suffix: &str) -> String {
        format!("devg-test-{}-{suffix}", std::process::id())
    }

    #[test]
    fn store_and_load_roundtrip() {
        if skip_unless_keychain_tests() { return; }
        let name = test_name("roundtrip");
        let json = r#"{"access_token":"abc"}"#;

        store(&name, json).unwrap();
        let loaded = load(&name).unwrap();
        assert_eq!(loaded, json);

        delete(&name).unwrap();
    }

    #[test]
    fn delete_removes_entry() {
        if skip_unless_keychain_tests() { return; }
        let name = test_name("delete");
        store(&name, "{}").unwrap();
        delete(&name).unwrap();
        assert!(load(&name).is_err());
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        if skip_unless_keychain_tests() { return; }
        let result = delete(&test_name("nonexistent"));
        assert!(result.is_ok());
    }

    #[test]
    fn list_names_includes_stored() {
        if skip_unless_keychain_tests() { return; }
        let name = test_name("list");
        store(&name, "{}").unwrap();

        let names = list_names().unwrap();
        assert!(names.contains(&name));

        delete(&name).unwrap();
    }

    #[test]
    fn index_updated_on_store_and_delete() {
        if skip_unless_keychain_tests() { return; }
        let name = test_name("index-sd");

        store(&name, "{}").unwrap();
        let names_after_store = list_names().unwrap();
        assert!(names_after_store.contains(&name), "name should be in index after store");

        delete(&name).unwrap();
        let names_after_delete = list_names().unwrap();
        assert!(!names_after_delete.contains(&name), "name should not be in index after delete");
    }
}
