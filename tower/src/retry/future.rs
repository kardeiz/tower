//! Future types

use super::{Policy, Retry};
use futures_core::ready;
use pin_project::{pin_project, project};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use tower_service::Service;

/// The `Future` returned by a `Retry` service.
#[pin_project]
#[derive(Debug)]
pub struct ResponseFuture<P, S, Request>
where
    P: Policy<Request, S::Response, S::Error>,
    S: Service<Request>,
{
    request: Option<Request>,
    #[pin]
    retry: Retry<P, S>,
    #[pin]
    state: State<S::Future, P::Future>,
}

#[pin_project]
#[derive(Debug)]
enum State<F, P> {
    /// Polling the future from `Service::call`
    Called(#[pin] F),
    /// Polling the future from `Policy::retry`
    Checking(#[pin] P),
    /// Polling `Service::poll_ready` after `Checking` was OK.
    Retrying,
}

impl<P, S, Request> ResponseFuture<P, S, Request>
where
    P: Policy<Request, S::Response, S::Error>,
    S: Service<Request>,
{
    pub(crate) fn new(
        request: Option<Request>,
        retry: Retry<P, S>,
        future: S::Future,
    ) -> ResponseFuture<P, S, Request> {
        ResponseFuture {
            request,
            retry,
            state: State::Called(future),
        }
    }
}

impl<P, S, Request> Future for ResponseFuture<P, S, Request>
where
    P: Policy<Request, S::Response, S::Error> + Clone,
    S: Service<Request> + Clone,
{
    type Output = Result<S::Response, S::Error>;

    #[project]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut this = self.project();

        loop {
            #[project]
            match this.state.as_mut().project() {
                State::Called(future) => {
                    let result = ready!(future.poll(cx));
                    if let Some(ref req) = this.request {
                        match this.retry.policy.retry(req, result.as_ref()) {
                            Some(checking) => {
                                this.state.set(State::Checking(checking));
                            }
                            None => return Poll::Ready(result),
                        }
                    } else {
                        // request wasn't cloned, so no way to retry it
                        return Poll::Ready(result);
                    }
                }
                State::Checking(future) => {
                    this.retry
                        .as_mut()
                        .project()
                        .policy
                        .set(ready!(future.poll(cx)));
                    this.state.set(State::Retrying);
                }
                State::Retrying => {
                    // NOTE: we assume here that
                    //
                    //   this.retry.poll_ready()
                    //
                    // is equivalent to
                    //
                    //   this.retry.service.poll_ready()
                    //
                    // we need to make that assumption to avoid adding an Unpin bound to the Policy
                    // in Ready to make it Unpin so that we can get &mut Ready as needed to call
                    // poll_ready on it.
                    ready!(this.retry.as_mut().project().service.poll_ready(cx))?;
                    let req = this
                        .request
                        .take()
                        .expect("retrying requires cloned request");
                    *this.request = this.retry.policy.clone_request(&req);
                    this.state.set(State::Called(
                        this.retry.as_mut().project().service.call(req),
                    ));
                }
            }
        }
    }
}
