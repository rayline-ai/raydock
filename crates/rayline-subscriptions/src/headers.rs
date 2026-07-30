use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HeaderSnapshot {
    values: BTreeMap<String, String>,
}

impl HeaderSnapshot {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl AsRef<str>, value: impl Into<String>) {
        self.values
            .insert(name.as_ref().to_ascii_lowercase(), value.into());
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values
            .get(&name.to_ascii_lowercase())
            .map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

impl<K, V> FromIterator<(K, V)> for HeaderSnapshot
where
    K: AsRef<str>,
    V: Into<String>,
{
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut headers = Self::new();
        for (name, value) in iter {
            headers.insert(name, value);
        }
        headers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_case_insensitive() {
        let headers = HeaderSnapshot::from_iter([("X-Test", "value".to_owned())]);
        assert_eq!(headers.get("x-test"), Some("value"));
        assert_eq!(headers.get("X-TEST"), Some("value"));
    }
}
