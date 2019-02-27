use std::collections::HashMap;

lazy_static! {
  static ref HOOKS: HashMap<String, &'static str> = {
    let mut map = HashMap::new();
    // include_str! only takes string literals, so we have to repeat ourselves. (rust-lang/rust#53749)
    map.insert("commit-msg".into(), include_str!("commit-msg"));
    // Intentionally omit pre-auto-gc.
    map
  };
}

pub fn hooks() -> &'static HashMap<String, &'static str> {
  &HOOKS
}
