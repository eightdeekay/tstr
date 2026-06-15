use std::collections::HashMap;
use std::fmt;

/// Runtime value — the result of evaluating any expression.
#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Value>),
    Object(HashMap<String, Value>),
}

impl Value {
    /// Convert a `serde_yaml::Value` to a tstr `Value`. Used when loading
    /// `constants:` from `tstr.yaml` into the constants namespace.
    /// Unknown types (tagged scalars, etc.) become Null.
    pub fn from_yaml(yaml: &serde_yaml::Value) -> Value {
        match yaml {
            serde_yaml::Value::Null => Value::Null,
            serde_yaml::Value::Bool(b) => Value::Bool(*b),
            serde_yaml::Value::Number(n) => {
                Value::Number(n.as_f64().unwrap_or(0.0))
            }
            serde_yaml::Value::String(s) => Value::String(s.clone()),
            serde_yaml::Value::Sequence(seq) => {
                Value::Array(seq.iter().map(Value::from_yaml).collect())
            }
            serde_yaml::Value::Mapping(map) => {
                let mut out = HashMap::new();
                for (k, v) in map {
                    if let Some(key) = k.as_str() {
                        out.insert(key.to_string(), Value::from_yaml(v));
                    }
                }
                Value::Object(out)
            }
            // Tagged scalars / other variants — treat as Null for now.
            _ => Value::Null,
        }
    }
}

impl Value {
    /// Is this value "truthy"? Used by assertions and the `|` operator.
    /// Null and false are falsy. Everything else is truthy.
    /// Empty strings and zero are truthy (unlike JS) — only null/false fail assertions.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            _ => true,
        }
    }

    /// Get a field from an object. Returns Null if not an object or field missing.
    /// Also handles built-in properties: .length on strings, .size on arrays/objects.
    pub fn get_field(&self, key: &str) -> Value {
        match self {
            Value::String(s) if key == "length" => Value::Number(s.len() as f64),
            Value::Array(arr) if key == "size" => Value::Number(arr.len() as f64),
            Value::Object(map) if key == "size" && !map.contains_key("size") => {
                Value::Number(map.len() as f64)
            }
            Value::Object(map) => map.get(key).cloned().unwrap_or(Value::Null),
            _ => Value::Null,
        }
    }

    /// Set a field on an object. Panics if not an object.
    pub fn set_field(&mut self, key: &str, value: Value) {
        match self {
            Value::Object(map) => { map.insert(key.to_string(), value); }
            _ => panic!("cannot set field '{}' on {:?}", key, self.type_name()),
        }
    }

    /// Get an array element by index. Negative indices count from end.
    pub fn get_index(&self, idx: i64) -> Value {
        match self {
            Value::Array(arr) => {
                let actual = if idx < 0 {
                    arr.len() as i64 + idx
                } else {
                    idx
                };
                if actual >= 0 && (actual as usize) < arr.len() {
                    arr[actual as usize].clone()
                } else {
                    Value::Null
                }
            }
            _ => Value::Null,
        }
    }

    /// Slice an array: [start:end]
    pub fn slice(&self, start: Option<i64>, end: Option<i64>) -> Value {
        match self {
            Value::Array(arr) => {
                let len = arr.len() as i64;
                let s = start.unwrap_or(0).max(0) as usize;
                let e = end.unwrap_or(len).min(len) as usize;
                if s <= e && s < arr.len() {
                    Value::Array(arr[s..e].to_vec())
                } else {
                    Value::Array(Vec::new())
                }
            }
            _ => Value::Null,
        }
    }

    /// Collect a field from all elements: items[].fieldName
    pub fn collect_field(&self, key: &str) -> Value {
        match self {
            Value::Array(arr) => {
                Value::Array(arr.iter().map(|item| item.get_field(key)).collect())
            }
            _ => Value::Null,
        }
    }

    /// Human-readable type name for error messages.
    pub fn type_name(&self) -> &str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// Convert to string for interpolation, regex, and display.
    pub fn to_display_string(&self) -> String {
        match self {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => {
                if *n == (*n as i64) as f64 {
                    format!("{}", *n as i64) // 200 not 200.0
                } else {
                    n.to_string()
                }
            }
            Value::String(s) => s.clone(),
            Value::Array(_) => format!("{}", self),
            Value::Object(_) => format!("{}", self),
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Value::Null, Value::Null) => true,
            (Value::Bool(a), Value::Bool(b)) => a == b,
            (Value::Number(a), Value::Number(b)) => a == b,
            (Value::String(a), Value::String(b)) => a == b,
            (Value::Array(a), Value::Array(b)) => a == b,
            _ => false,
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self, other) {
            (Value::Number(a), Value::Number(b)) => a.partial_cmp(b),
            (Value::String(a), Value::String(b)) => a.partial_cmp(b),
            _ => None,
        }
    }
}

/// Display as JSON-ish format.
impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{}", b),
            Value::Number(n) => {
                if *n == (*n as i64) as f64 {
                    write!(f, "{}", *n as i64)
                } else {
                    write!(f, "{}", n)
                }
            }
            Value::String(s) => write!(f, "\"{}\"", s),
            Value::Array(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "{}", item)?;
                }
                write!(f, "]")
            }
            Value::Object(map) => {
                write!(f, "{{")?;
                let mut keys: Vec<_> = map.keys().collect();
                keys.sort();
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 { write!(f, ", ")?; }
                    write!(f, "\"{}\": {}", key, map[*key])?;
                }
                write!(f, "}}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truthiness() {
        assert!(!Value::Null.is_truthy());
        assert!(!Value::Bool(false).is_truthy());
        assert!(Value::Bool(true).is_truthy());
        assert!(Value::Number(0.0).is_truthy());
        assert!(Value::Number(42.0).is_truthy());
        assert!(Value::String("".to_string()).is_truthy());
        assert!(Value::String("hello".to_string()).is_truthy());
        assert!(Value::Array(vec![]).is_truthy());
    }

    #[test]
    fn test_get_field() {
        let mut map = HashMap::new();
        map.insert("name".to_string(), Value::String("Test".to_string()));
        map.insert("count".to_string(), Value::Number(3.0));
        let obj = Value::Object(map);

        assert_eq!(obj.get_field("name"), Value::String("Test".to_string()));
        assert_eq!(obj.get_field("count"), Value::Number(3.0));
        assert_eq!(obj.get_field("missing"), Value::Null);
    }

    #[test]
    fn test_string_length() {
        let s = Value::String("hello".to_string());
        assert_eq!(s.get_field("length"), Value::Number(5.0));
    }

    #[test]
    fn test_array_size() {
        let arr = Value::Array(vec![Value::Number(1.0), Value::Number(2.0)]);
        assert_eq!(arr.get_field("size"), Value::Number(2.0));
    }

    #[test]
    fn test_get_index() {
        let arr = Value::Array(vec![
            Value::String("a".to_string()),
            Value::String("b".to_string()),
            Value::String("c".to_string()),
        ]);
        assert_eq!(arr.get_index(0), Value::String("a".to_string()));
        assert_eq!(arr.get_index(2), Value::String("c".to_string()));
        assert_eq!(arr.get_index(-1), Value::String("c".to_string()));
        assert_eq!(arr.get_index(5), Value::Null);
    }

    #[test]
    fn test_slice() {
        let arr = Value::Array(vec![
            Value::Number(1.0), Value::Number(2.0),
            Value::Number(3.0), Value::Number(4.0),
        ]);
        assert_eq!(
            arr.slice(Some(0), Some(2)),
            Value::Array(vec![Value::Number(1.0), Value::Number(2.0)])
        );
    }

    #[test]
    fn test_collect_field() {
        let arr = Value::Array(vec![
            Value::Object(HashMap::from([
                ("id".to_string(), Value::Number(1.0)),
            ])),
            Value::Object(HashMap::from([
                ("id".to_string(), Value::Number(2.0)),
            ])),
        ]);
        assert_eq!(
            arr.collect_field("id"),
            Value::Array(vec![Value::Number(1.0), Value::Number(2.0)])
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(Value::Null.to_string(), "null");
        assert_eq!(Value::Number(200.0).to_display_string(), "200");
        assert_eq!(Value::Number(3.14).to_display_string(), "3.14");
        assert_eq!(Value::String("hello".to_string()).to_display_string(), "hello");
    }

    #[test]
    fn test_equality() {
        assert_eq!(Value::Null, Value::Null);
        assert_eq!(Value::Number(42.0), Value::Number(42.0));
        assert_ne!(Value::Number(42.0), Value::String("42".to_string()));
        assert_ne!(Value::Null, Value::Bool(false));
    }

    #[test]
    fn test_comparison() {
        assert!(Value::Number(10.0) > Value::Number(5.0));
        assert!(Value::String("b".to_string()) > Value::String("a".to_string()));
    }
}
