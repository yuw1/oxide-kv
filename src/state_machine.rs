use std::collections::HashMap;
use std::io::{self};

pub struct StateMachine {
    index: HashMap<String, String>
}

impl StateMachine {
    pub fn open() -> io::Result<Self> {
        Ok(StateMachine { index: HashMap::new() })
    }

    pub fn set(&mut self, key: &str, value: &str) -> io::Result<()> {
        self.index.insert(key.to_string(), value.to_string());
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&String> {
        self.index.get(key)
    }

    pub fn delete(&mut self, key: &str) -> io::Result<()> {
        self.index.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_empty_state_machine() {
        let sm = StateMachine::open().unwrap();
        assert!(sm.get("anything").is_none());
    }

    #[test]
    fn set_then_get_returns_value() {
        let mut sm = StateMachine::open().unwrap();
        sm.set("hello", "world").unwrap();
        assert_eq!(sm.get("hello"), Some(&"world".to_string()));
    }

    #[test]
    fn get_missing_key_returns_none() {
        let sm = StateMachine::open().unwrap();
        assert!(sm.get("missing").is_none());
    }

    #[test]
    fn set_overwrites_existing_value() {
        let mut sm = StateMachine::open().unwrap();
        sm.set("k", "v1").unwrap();
        sm.set("k", "v2").unwrap();
        assert_eq!(sm.get("k"), Some(&"v2".to_string()));
    }

    #[test]
    fn delete_removes_existing_key() {
        let mut sm = StateMachine::open().unwrap();
        sm.set("k", "v").unwrap();
        sm.delete("k").unwrap();
        assert!(sm.get("k").is_none());
    }

    #[test]
    fn delete_missing_key_is_noop() {
        let mut sm = StateMachine::open().unwrap();
        // Should not panic or error.
        sm.delete("never_set").unwrap();
    }

    #[test]
    fn delete_then_set_works() {
        let mut sm = StateMachine::open().unwrap();
        sm.set("k", "v1").unwrap();
        sm.delete("k").unwrap();
        sm.set("k", "v2").unwrap();
        assert_eq!(sm.get("k"), Some(&"v2".to_string()));
    }
}