//! The rule of the Wi-Fi task for a request that arrives during a join.

/// Work for the Wi-Fi task.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WifiRequest {
    /// Join the network, and answer with this token.
    On {
        /// The token of the request in the shell.
        token: u32,
    },
    /// Leave the network.
    Off,
}

/// What a join in progress does with a new request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DuringJoin {
    /// Continue the join, and answer with this token. The answer is also for
    /// the older requests.
    Continue {
        /// The token of the answer.
        token: u32,
    },
    /// Stop the join, and do not answer. The shell already answered the
    /// requests that wait.
    Cancel,
}

/// Returns what a join for `token` does when `request` arrives.
#[must_use]
pub fn during_join(token: u32, request: WifiRequest) -> DuringJoin {
    match request {
        WifiRequest::On { token: newer } => DuringJoin::Continue {
            token: token.max(newer),
        },
        WifiRequest::Off => DuringJoin::Cancel,
    }
}

#[cfg(test)]
mod tests {
    use super::{DuringJoin, WifiRequest, during_join};

    #[test]
    fn wifi_off_cancels_a_join() {
        assert_eq!(during_join(3, WifiRequest::Off), DuringJoin::Cancel);
    }

    #[test]
    fn a_new_request_continues_the_join_with_its_token() {
        assert_eq!(
            during_join(3, WifiRequest::On { token: 4 }),
            DuringJoin::Continue { token: 4 }
        );
        assert_eq!(
            during_join(3, WifiRequest::On { token: 2 }),
            DuringJoin::Continue { token: 3 }
        );
    }
}
