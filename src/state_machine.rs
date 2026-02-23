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