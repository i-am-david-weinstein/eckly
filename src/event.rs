use std::time::Duration;

use crossterm::event::{self, Event as CrosstermEvent, KeyEvent};
use image::DynamicImage;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{ApiError, OAuthDevice};
use crate::article::Article;
use crate::graphics::Graphic;
use crate::models::{
    Bookmark, BookmarkPatch, Collection, Label, Page, Profile,
};

/// Everything that drives the application from the outside world.
#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Paste(String),
    Resize,
    Tick,
    Api(Box<ApiEvent>),
}

impl Event {
    pub fn api(event: ApiEvent) -> Self {
        Self::Api(Box::new(event))
    }
}

/// Results of background requests.
#[derive(Debug)]
pub enum ApiEvent {
    Bookmarks {
        seq: u64,
        append: bool,
        result: Result<Page<Bookmark>, ApiError>,
    },
    Labels(Result<Vec<Label>, ApiError>),
    Collections(Result<Vec<Collection>, ApiError>),
    Profile(Result<Profile, ApiError>),
    Article {
        id: String,
        result: Result<Article, ApiError>,
    },
    Annotations {
        bookmark_id: String,
        result: Result<Vec<crate::models::Annotation>, ApiError>,
    },
    AnnotationCreated {
        bookmark_id: String,
        result: Result<crate::models::Annotation, ApiError>,
    },
    AnnotationUpdated {
        bookmark_id: String,
        id: String,
        color: String,
        note: String,
        result: Result<(), ApiError>,
    },
    AnnotationDeleted {
        bookmark_id: String,
        id: String,
        result: Result<(), ApiError>,
    },
    AnnotationSummaries(Result<Vec<crate::models::Annotation>, ApiError>),
    Image {
        bookmark_id: String,
        url: String,
        result: Result<DynamicImage, ApiError>,
    },
    ImageEncoded {
        bookmark_id: String,
        url: String,
        width: usize,
        result: Result<Graphic, String>,
    },
    Updated {
        id: String,
        patch: BookmarkPatch,
        message: String,
        quiet: bool,
        result: Result<(), ApiError>,
    },
    LabelRenamed {
        from: String,
        to: String,
        result: Result<(), ApiError>,
    },
    BulkUpdated {
        ids: Vec<String>,
        patch: BookmarkPatch,
        message: String,
        quiet: bool,
        result: Result<usize, ApiError>,
    },
    Created(Result<Option<String>, ApiError>),
    Deleted {
        id: String,
        result: Result<(), ApiError>,
    },
    BulkDeleted {
        ids: Vec<String>,
        result: Result<usize, ApiError>,
    },
    Setup(Result<Profile, ApiError>),
    OAuthDevice {
        seq: u64,
        result: Result<OAuthDevice, ApiError>,
    },
    OAuthToken {
        seq: u64,
        result: Result<String, ApiError>,
    },
    PendingSynced(PendingSyncResult),
}

#[derive(Debug, Default)]
pub struct PendingSyncResult {
    pub updated: Vec<String>,
    pub deleted: Vec<String>,
    pub errors: Vec<String>,
    pub network_error: bool,
}

const TICK: Duration = Duration::from_millis(120);

/// Read terminal input on a dedicated thread so the async event loop can
/// remain responsive to API results.
pub fn spawn_input_thread(tx: UnboundedSender<Event>) {
    std::thread::spawn(move || {
        loop {
            match event::poll(TICK) {
                Ok(true) => match event::read() {
                    Ok(CrosstermEvent::Key(key)) => {
                        if tx.send(Event::Key(key)).is_err() {
                            break;
                        }
                    }
                    Ok(CrosstermEvent::Paste(text)) => {
                        if tx.send(Event::Paste(text)).is_err() {
                            break;
                        }
                    }
                    Ok(CrosstermEvent::Resize(_, _)) => {
                        if tx.send(Event::Resize).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
                Ok(false) => {
                    if tx.send(Event::Tick).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}
