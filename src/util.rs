use regex::Regex;

lazy_static::lazy_static! {
    pub static ref COMP_KEY_RE: Regex = Regex::new(r"^[a-zA-Z][\w\.-]*\.[\w\.-]+$").unwrap();
}
