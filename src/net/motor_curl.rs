//! Motor OS HTTP transport using the image's curl binary.

use super::curl::CurlTransport;
use super::{EgressPolicy, HttpClient, HttpRequest, HttpSink, NetError, ResponseHead};
use std::path::Path;

/// Where the image installs curl. An absolute path on purpose: Motor OS
/// spawns take the name as given, with no PATH search to lean on.
pub const MOTOR_CURL: &str = "/system/bin/curl";

pub struct MotorCurl {
    transport: CurlTransport,
}

impl MotorCurl {
    pub fn new(policy: EgressPolicy) -> Result<MotorCurl, NetError> {
        Ok(MotorCurl::with_program(MOTOR_CURL, policy))
    }

    /// A different curl binary, for tests that need a stand-in.
    pub fn with_program(program: &str, policy: EgressPolicy) -> MotorCurl {
        MotorCurl {
            transport: CurlTransport::new(program, policy),
        }
    }

    /// Supply the value for a secret header's environment variable. The
    /// value is registered for redaction, so it cannot appear in the trace.
    pub fn with_secret(mut self, env: &str, value: &str) -> MotorCurl {
        self.transport.add_secret(env, value);
        self
    }

    pub fn with_verbosity(mut self, level: u8) -> MotorCurl {
        self.transport.set_verbosity(level, true);
        self
    }

    pub fn with_ca_cert(mut self, path: &Path) -> Result<MotorCurl, NetError> {
        let path = path
            .to_str()
            .ok_or_else(|| NetError::BadRequest("provider CA path is not UTF-8".to_string()))?;
        self.transport.set_ca_cert(path);
        Ok(self)
    }
}

impl HttpClient for MotorCurl {
    fn execute(
        &self,
        req: &HttpRequest,
        sink: &mut dyn HttpSink,
    ) -> Result<ResponseHead, NetError> {
        self.transport.execute(req, sink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{CollectSink, Url};

    fn policy() -> EgressPolicy {
        EgressPolicy::new(&["openrouter.ai".to_string()])
    }

    /// Egress is checked before any process is spawned: a blocked host is
    /// blocked, and an allowed one fails on the missing binary — in that
    /// order.
    #[test]
    fn egress_answers_before_the_program_is_even_looked_for() {
        let client = MotorCurl::with_program("gears-no-such-curl", policy());
        let mut sink = CollectSink::default();

        let req = HttpRequest::get(Url::parse("https://evil.test/x").unwrap());
        assert!(matches!(
            client.execute(&req, &mut sink),
            Err(NetError::Forbidden(_))
        ));

        let req = HttpRequest::get(Url::parse("https://openrouter.ai/api").unwrap());
        let err = client.execute(&req, &mut sink).unwrap_err();
        assert!(matches!(err, NetError::Transport(_)), "{err}");
        assert!(err.to_string().contains("cannot run"), "{err}");
        assert!(sink.head.is_none(), "nothing must reach the sink");
    }
}
