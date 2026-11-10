use crate::{
    chrome::{
        Candidate, Chrome, Membership, PlaylistChoice, PlaylistDetails, SaveFailure, Search,
        SearchFailure,
    },
    report::{Part, Result},
    soundcloud::SoundCloud,
};

pub enum Destination {
    Browser(Box<Chrome>),
    Api(Box<SoundCloud>),
}

macro_rules! dispatch {
    ($($name:ident($($arg:ident: $ty:ty),*) -> $result:ty;)*) => {$(
        pub fn $name(&mut self, $($arg: $ty,)* check: &impl Fn() -> Result<()>) -> $result {
            match self {
                Self::Browser(browser) => browser.$name($($arg,)* check),
                Self::Api(api) => api.$name($($arg,)* check),
            }
        }
    )*};
}

impl Destination {
    pub fn browser(&mut self) -> Option<&mut Chrome> {
        match self {
            Self::Browser(browser) => Some(browser),
            Self::Api(_) => None,
        }
    }

    pub fn disconnected(&mut self) -> bool {
        self.browser().is_some_and(Chrome::disconnected)
    }

    dispatch! {
        search(query: &str) -> std::result::Result<Search, SearchFailure>;
        library() -> Result<Vec<PlaylistChoice>>;
        playlist_details(url: &str) -> Result<PlaylistDetails>;
        playlist_links(names: &[String]) -> Result<Vec<Option<String>>>;
        has_track(track: &str, part: &Part) -> Result<bool>;
        open_candidate(candidate: &Candidate) -> Result<()>;
        probe_open_part(part: &Part) -> Result<Option<Membership>>;
        prepare_open_part(part: &Part) -> Result<bool>;
        prepare(candidate: &Candidate, part: &Part) -> Result<bool>;
        save(part: &mut Part) -> std::result::Result<(), SaveFailure>;
        remove_previous(previous: &str, replacement: &str, part: &Part) -> Result<()>;
    }
}
