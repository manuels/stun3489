use std::io::Error;
use std::time::Duration;

use futures::Future;
use futures::BoxFuture;
use futures::IntoFuture;

use tokio_core::reactor::Timeout;
use tokio_core::reactor::Handle;

use std::convert;

pub trait IntoTimeoutFuture<F:Future + Send> {
    fn timeout(self, handle: &Handle, duration: Duration) -> BoxFuture<Option<F::Item>, F::Error>;
}

impl<F: Future + Send + 'static> IntoTimeoutFuture<F> for F
    where F::Item: Send,
          F::Error: convert::From<Error> + Send
{
    fn timeout(self, handle: &Handle, duration: Duration) -> BoxFuture<Option<F::Item>, F::Error> {
        let timeout = Timeout::new(duration, handle).into_future().flatten();

        let future = self.map(Some);
        let timeout = timeout.map(|_| None)
                             .map_err(|e| e.into());

        future.select(timeout).then(|r| {
            match r {
                Ok((option, _)) => Ok(option),
                Err((err, _)) => Err(err),
            }
        }).boxed()
    }
}

