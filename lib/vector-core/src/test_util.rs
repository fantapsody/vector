use futures::{task::noop_waker_ref, Stream, StreamExt};
use std::{
    fs::File,
    path::Path,
    task::{Context, Poll},
};

pub fn open_fixture(path: impl AsRef<Path>) -> crate::Result<serde_json::Value> {
    serde_json::from_reader(File::open(path)?).map_err(Into::into)
}

pub fn collect_ready<S>(mut rx: S) -> Vec<S::Item>
where
    S: Stream + Unpin,
{
    let waker = noop_waker_ref();
    let mut cx = Context::from_waker(waker);

    let mut vec = Vec::new();
    loop {
        match rx.poll_next_unpin(&mut cx) {
            Poll::Ready(Some(item)) => vec.push(item),
            Poll::Ready(None) | Poll::Pending => return vec,
        }
    }
}
