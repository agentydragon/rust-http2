#![allow(dead_code)]

use futures::stream::Stream;
use futures::Async;
use futures::Poll;

use crate::solicit::DEFAULT_SETTINGS;

use crate::error;

use super::stream_queue_sync::StreamQueueSyncReceiver;
use super::types::Types;
use crate::common::increase_in_window::IncreaseInWindow;
use crate::data_or_headers::DataOrHeaders;
use crate::data_or_headers_with_flag::DataOrHeadersWithFlag;

/// Stream that provides data from network.
/// Most importantly, it increases WINDOW.
pub(crate) struct StreamFromNetwork<T: Types> {
    pub rx: StreamQueueSyncReceiver<T>,
    pub increase_in_window: IncreaseInWindow<T>,
}

impl<T: Types> Stream for StreamFromNetwork<T> {
    type Item = DataOrHeadersWithFlag;
    type Error = error::Error;

    fn poll(&mut self) -> Poll<Option<DataOrHeadersWithFlag>, error::Error> {
        let part = match self.rx.poll() {
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Err(e) => return Err(e),
            Ok(Async::Ready(None)) => return Ok(Async::Ready(None)),
            Ok(Async::Ready(Some(part))) => part,
        };

        if let DataOrHeadersWithFlag {
            content: DataOrHeaders::Data(ref b),
            ..
        } = part
        {
            self.increase_in_window.data_frame_processed(b.len() as u32);

            // TODO: use different
            // TODO: increment after process of the frame (i. e. on next poll)
            let edge = DEFAULT_SETTINGS.initial_window_size / 2;
            if self.increase_in_window.in_window_size() < edge {
                let inc = DEFAULT_SETTINGS.initial_window_size;
                self.increase_in_window.increase_window(inc)?;
            }
        }

        Ok(Async::Ready(Some(part)))
    }
}

impl<T: Types> Drop for StreamFromNetwork<T> {
    fn drop(&mut self) {
        // TODO: reset stream
    }
}
