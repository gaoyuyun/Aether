use std::io;
use std::net::SocketAddr;
use std::time::Duration;

/// Maximum number of addresses accepted from one hostname lookup.
///
/// A DNS response is attacker-controlled at the resolver boundary.  Keeping
/// the result bounded prevents a pathological answer from forcing an
/// unbounded vector allocation or an unbounded connect fan-out.  Callers still
/// validate every returned address for their own network policy.
pub const MAX_DNS_RESOLVED_ADDRESSES: usize = 32;

/// Upper bound used by callers that do not have a tighter request deadline.
pub const DEFAULT_DNS_LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolve a host while bounding both resolver wait time and answer count.
///
/// The iterator is consumed one item past the allowed count so an answer set
/// larger than the policy is rejected rather than silently truncated.  This
/// keeps validation and connection pinning based on the complete, bounded
/// answer set.
pub async fn lookup_host_with_limits(
    host: &str,
    port: u16,
    timeout: Duration,
) -> io::Result<Vec<SocketAddr>> {
    if timeout.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DNS lookup timeout must be non-zero",
        ));
    }

    let mut resolved = tokio::time::timeout(timeout, tokio::net::lookup_host((host, port)))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "DNS lookup timed out"))??;

    collect_resolved_addresses_with_limit(&mut resolved)
}

fn collect_resolved_addresses_with_limit(
    resolved: &mut impl Iterator<Item = SocketAddr>,
) -> io::Result<Vec<SocketAddr>> {
    let mut addresses = Vec::with_capacity(MAX_DNS_RESOLVED_ADDRESSES.min(8));
    for address in resolved.by_ref() {
        if addresses.len() >= MAX_DNS_RESOLVED_ADDRESSES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "DNS lookup returned too many addresses",
            ));
        }
        addresses.push(address);
    }
    Ok(addresses)
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    use super::{
        collect_resolved_addresses_with_limit, lookup_host_with_limits, DEFAULT_DNS_LOOKUP_TIMEOUT,
        MAX_DNS_RESOLVED_ADDRESSES,
    };

    #[tokio::test]
    async fn rejects_zero_dns_timeout_before_resolving() {
        let error = lookup_host_with_limits("localhost", 80, std::time::Duration::ZERO)
            .await
            .expect_err("zero timeout must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[tokio::test]
    async fn resolves_within_shared_address_bound() {
        let addresses = lookup_host_with_limits("localhost", 80, DEFAULT_DNS_LOOKUP_TIMEOUT)
            .await
            .expect("localhost should resolve in the test environment");
        assert!(!addresses.is_empty());
        assert!(addresses.len() <= MAX_DNS_RESOLVED_ADDRESSES);
    }

    #[test]
    fn rejects_an_answer_set_larger_than_the_shared_bound() {
        let mut resolved = (0..=MAX_DNS_RESOLVED_ADDRESSES)
            .map(|index| SocketAddr::from(([192, 0, 2, (index % 254 + 1) as u8], 443)));
        let error = collect_resolved_addresses_with_limit(&mut resolved)
            .expect_err("more than 32 DNS answers must be rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
