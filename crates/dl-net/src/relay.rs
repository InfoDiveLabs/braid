//! Lanes that reach the origin through a relay rather than through a local
//! interface.
//!
//! A relay is a machine on the network forwarding for us: in practice an
//! Android phone running the companion app, which binds its sockets to the
//! cellular network and so reaches the origin over mobile data while itself
//! sitting on the local Wi-Fi. Several phones are several independent paths to
//! the internet, which is the same thing multi-NIC gives, arrived at from the
//! other direction.
//!
//! **Nothing here binds a socket.** A relay lane is an ordinary HTTP client
//! pointed at a different *address*, so none of `SO_BINDTODEVICE`,
//! `IP_BOUND_IF` or `IP_UNICAST_IF` is involved. That is most of the appeal:
//! the binding matrix is the least portable part of this project and the only
//! piece that cannot be verified on the machine it was written on, and this
//! path needs none of it. It works the same on all three desktops and needs no
//! privileges.
//!
//! What a relay shares with an interface is everything that matters to the
//! engine: it is a byte source, it has its own public address so per-source
//! signed URLs resolve per lane, and it can go away mid-transfer: a phone
//! loses signal, sleeps, or throttles itself when hot: which the lane selector
//! already handles by parking it and moving the work.
//!
//! The lanes themselves live in [`crate::path`], because an engine weighing
//! one lane against another has no reason to care which of them is a card and
//! which is a phone.

/// One relay, as the desktop knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relay {
    /// What to call it in the sidebar. The phone's name, usually.
    pub name: String,
    /// Where it listens, as `host:port`.
    pub address: String,
    /// What pairing handed back. Absent for a relay that has been seen but not
    /// authorised, which may be asked who it is and nothing else.
    pub key: Option<String>,
}

impl Relay {
    pub fn new(name: impl Into<String>, address: impl Into<String>, key: Option<String>) -> Self {
        Self { name: name.into(), address: address.into(), key }
    }

    /// As a proxy URL, with the key as credentials.
    ///
    /// Credentials in the URL is how `ProxyMode::Manual` already carries them,
    /// so nothing in the HTTP layer needs to learn that relays exist.
    ///
    /// The username is the network the phone should use. It has to travel on
    /// every request, and proxy credentials are the one field that already
    /// does, so this avoids inventing a header the phone would have to be
    /// taught separately.
    /// An address may arrive either way: typed by a person as `host:port`, or
    /// handed over by discovery as a full URL. Prepending a scheme
    /// unconditionally produces `http://http://host`, which fails as a DNS
    /// lookup of the word "http" and is a confusing way to learn this.
    pub fn proxy_url(&self, network: &str) -> String {
        let (scheme, host) = match self.address.split_once("://") {
            Some((scheme, rest)) => (scheme, rest),
            None => ("http", self.address.as_str()),
        };
        match &self.key {
            Some(key) => format!("{scheme}://{network}:{key}@{host}"),
            None => format!("{scheme}://{host}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_paired_relay_carries_its_key_as_credentials() {
        let relay = Relay::new("Pixel", "10.0.0.5:8710", Some("secret".into()));
        assert_eq!(relay.proxy_url("cell"), "http://cell:secret@10.0.0.5:8710");
    }

    #[test]
    fn an_unpaired_relay_offers_no_credentials() {
        // It will earn a 407, which is the correct outcome: the alternative is
        // inventing a key and being refused anyway, more confusingly.
        let relay = Relay::new("Pixel", "10.0.0.5:8710", None);
        assert_eq!(relay.proxy_url("cell"), "http://10.0.0.5:8710");
    }

    #[test]
    fn an_address_that_already_has_a_scheme_does_not_get_another() {
        // Discovery hands over a full URL, a person types `host:port`, and
        // both have to work.
        let relay = Relay::new("Pixel", "http://10.0.0.5:8710", Some("secret".into()));
        assert_eq!(relay.proxy_url("cell"), "http://cell:secret@10.0.0.5:8710");

        let bare = Relay::new("Pixel", "10.0.0.5:8710", Some("secret".into()));
        assert_eq!(bare.proxy_url("cell"), relay.proxy_url("cell"));
    }

    #[test]
    fn the_network_travels_in_the_username() {
        // Two lanes on one phone differ only here, so it is the field that
        // decides which radio serves the request.
        let relay = Relay::new("Pixel", "10.0.0.5:8710", Some("k".into()));
        assert_ne!(relay.proxy_url("cell"), relay.proxy_url("wifi"));
    }
}
