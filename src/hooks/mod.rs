use std::{collections::HashMap, sync::OnceLock};

pub fn hooks() -> &'static HashMap<&'static str, &'static str> {
  static HOOKS: OnceLock<HashMap<&'static str, &'static str>> = OnceLock::new();
  HOOKS.get_or_init(|| {
    let mut map = HashMap::new();
    // include_str! only takes string literals, so we have to repeat ourselves. (rust-lang/rust#53749)
    map.insert("commit-msg", include_str!("commit-msg"));
    // Intentionally omit pre-auto-gc.
    map
  })
}
