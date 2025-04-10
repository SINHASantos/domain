use core::convert::From;

use super::nsec::GenerateNsecConfig;
use super::nsec3::GenerateNsec3Config;
use crate::dnssec::sign::records::DefaultSorter;
use octseq::{EmptyBuilder, FromBuilder};

//------------ DenialConfig --------------------------------------------------

/// Authenticated denial of existence configuration for a DNSSEC signed zone.
///
/// A DNSSEC signed zone must have either `NSEC` or `NSEC3` records to enable
/// the server to authenticate responses for names or record types that are
/// not present in the zone.
///
/// This type can be used to choose which denial mechanism should be used when
/// DNSSEC signing a zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DenialConfig<O, Sort = DefaultSorter>
where
    O: AsRef<[u8]> + From<&'static [u8]>,
{
    /// The zone already has the necessary NSEC(3) records.
    AlreadyPresent,

    /// The zone already has NSEC records.
    Nsec(GenerateNsecConfig),

    /// The zone already has NSEC3 records.
    ///
    /// Note: While zones can have multiple NSEC3 chains, only the configuraton
    /// for a single chain can be expressed using this type.
    ///
    /// https://datatracker.ietf.org/doc/html/rfc5155#section-7.3
    /// 7.3.  Secondary Servers
    ///   ...
    ///   "If there are multiple NSEC3PARAM RRs present, there are multiple
    ///    valid NSEC3 chains present.  The server must choose one of them,
    ///    but may use any criteria to do so."
    ///
    /// https://datatracker.ietf.org/doc/html/rfc5155#section-12.1.3
    /// 12.1.3.  Transitioning to a New Hash Algorithm
    ///   "Although the NSEC3 and NSEC3PARAM RR formats include a hash
    ///    algorithm parameter, this document does not define a particular
    ///    mechanism for safely transitioning from one NSEC3 hash algorithm to
    ///    another.  When specifying a new hash algorithm for use with NSEC3,
    ///    a transition mechanism MUST also be defined.  It is possible that
    ///    the only practical and palatable transition mechanisms may require
    ///    an intermediate transition to an insecure state, or to a state that
    ///    uses NSEC records instead of NSEC3."
    Nsec3(GenerateNsec3Config<O, Sort>),
}

impl<O> Default for DenialConfig<O, DefaultSorter>
where
    O: AsRef<[u8]> + From<&'static [u8]> + FromBuilder,
    <O as FromBuilder>::Builder: EmptyBuilder + AsRef<[u8]> + AsMut<[u8]>,
{
    fn default() -> Self {
        Self::Nsec(GenerateNsecConfig::default())
    }
}
