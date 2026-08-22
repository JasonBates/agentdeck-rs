#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProtocolSupport {
    TooOld { protocol: u32 },
    Baseline { protocol: u32 },
    Current { protocol: u32 },
    FutureUntested { protocol: u32 },
}

impl ProtocolSupport {
    pub fn is_usable(self) -> bool {
        !matches!(self, Self::TooOld { .. })
    }

    pub fn warning(self) -> Option<String> {
        match self {
            Self::FutureUntested { protocol } => Some(format!(
                "Herdr protocol {protocol} is newer than the verified protocols 19 and 20; continuing because the required snapshot subset decoded"
            )),
            _ => None,
        }
    }

    pub fn diagnostic(self, version: &str) -> String {
        match self {
            Self::TooOld { protocol } => format!(
                "Herdr {version} uses protocol {protocol}, older than AgentDeck's minimum protocol 19"
            ),
            Self::Baseline { protocol } => {
                format!("Herdr {version} protocol {protocol}: supported baseline")
            }
            Self::Current { protocol } => {
                format!("Herdr {version} protocol {protocol}: verified current")
            }
            Self::FutureUntested { protocol } => format!(
                "Herdr {version} protocol {protocol}: untested future protocol; required snapshot subset decoded"
            ),
        }
    }
}

pub fn assess_protocol(protocol: u32) -> ProtocolSupport {
    match protocol {
        0..=18 => ProtocolSupport::TooOld { protocol },
        19 => ProtocolSupport::Baseline { protocol },
        20 => ProtocolSupport::Current { protocol },
        protocol => ProtocolSupport::FutureUntested { protocol },
    }
}

#[cfg(test)]
mod tests {
    use super::{ProtocolSupport, assess_protocol};

    #[test]
    fn protocol_policy_distinguishes_baseline_current_future_and_too_old() {
        assert_eq!(
            assess_protocol(18),
            ProtocolSupport::TooOld { protocol: 18 }
        );
        assert_eq!(
            assess_protocol(19),
            ProtocolSupport::Baseline { protocol: 19 }
        );
        assert_eq!(
            assess_protocol(20),
            ProtocolSupport::Current { protocol: 20 }
        );
        assert_eq!(
            assess_protocol(21),
            ProtocolSupport::FutureUntested { protocol: 21 }
        );
        assert!(!assess_protocol(18).is_usable());
        assert!(assess_protocol(19).is_usable());
        assert!(assess_protocol(21).warning().is_some());
        assert!(assess_protocol(18).diagnostic("0.7.0").contains("minimum"));
    }
}
