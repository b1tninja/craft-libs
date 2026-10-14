//! SOAP 1.2 faults, parsed into a service-neutral value.
//!
//! A WSD service reports problems as a fault whose sub-code names the cause. This module reads the
//! structure and leaves the meaning of the sub-code (a namespace plus a local name) to the service
//! crate, which maps it onto its own enumeration.

use std::fmt;

use roxmltree::Node;

use crate::ns;
use crate::xml;

/// A resolved sub-code QName: the namespace (if the prefix was declared) and the local name.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SubCode {
    pub ns: Option<String>,
    pub local: String,
}

impl SubCode {
    /// Is this exactly `{ns}local`? Prefixes never matter, namespaces do.
    pub fn is(&self, ns_uri: &str, local: &str) -> bool {
        self.ns.as_deref() == Some(ns_uri) && self.local == local
    }

    /// `wsa:ActionNotSupported`, in either WS-Addressing namespace.
    pub fn is_action_not_supported(&self) -> bool {
        self.local == "ActionNotSupported" && (self.ns.as_deref() == Some(ns::WSA) || self.ns.as_deref() == Some(ns::WSA_FINAL))
    }
}

/// Who the fault blames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// `soap:Sender`: the request was wrong.
    Sender,
    /// `soap:Receiver`: the device failed.
    Receiver,
    /// Another, or missing, top-level code.
    Other,
}

/// A parsed fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fault {
    pub role: Role,
    pub code: SubCode,
    /// The first reason text, capped.
    pub reason: String,
    /// The text of the detail, if any, capped.
    pub detail: String,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the device refused: {}", self.code.local)?;
        if !self.reason.is_empty() {
            write!(f, " ({})", self.reason)?;
        }
        if !self.detail.is_empty() {
            write!(f, " [{}]", self.detail)?;
        }
        Ok(())
    }
}

/// Read a `soap:Fault` element. Missing parts degrade to empty values; this never fails.
pub fn parse_fault(fault: Node<'_, '_>) -> Fault {
    let code_el = xml::child(fault, ns::SOAP, "Code");
    let role = code_el.and_then(|c| xml::child(c, ns::SOAP, "Value")).and_then(|v| xml::resolve_qname(v, &xml::text(v))).map_or(
        Role::Other,
        |(uri, local)| match (uri.as_deref(), local.as_str()) {
            (Some(ns::SOAP), "Sender") => Role::Sender,
            (Some(ns::SOAP), "Receiver") => Role::Receiver,
            _ => Role::Other,
        },
    );
    // The first sub-code that names something; nested sub-codes are refinements we do not need.
    let code = code_el
        .and_then(|c| xml::child(c, ns::SOAP, "Subcode"))
        .and_then(|s| xml::child(s, ns::SOAP, "Value"))
        .and_then(|v| xml::resolve_qname(v, &xml::text(v)))
        .map_or_else(SubCode::default, |(ns, local)| SubCode { ns, local });
    let reason = xml::child(fault, ns::SOAP, "Reason").and_then(|r| xml::child(r, ns::SOAP, "Text")).map(xml::text).unwrap_or_default();
    let detail = xml::child(fault, ns::SOAP, "Detail")
        .map(|d| {
            let mut parts: Vec<String> = Vec::new();
            for n in d.descendants().filter(|n| n.is_text()).take(32) {
                let t = n.text().unwrap_or("").trim();
                if !t.is_empty() {
                    parts.push(t.to_owned());
                }
            }
            parts.join(" ").chars().take(xml::MAX_TEXT).collect()
        })
        .unwrap_or_default();
    Fault { role, code, reason: reason.chars().take(512).collect(), detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVC: &str = "urn:example:service";

    fn fault_xml(sub: &str, extra_ns: &str) -> String {
        format!(
            "<s:Envelope xmlns:s=\"{}\" xmlns:svc=\"{SVC}\" xmlns:wsa=\"{}\"{extra_ns}><s:Body><s:Fault><s:Code><s:Value>s:Sender</s:Value><s:Subcode><s:Value>{sub}</s:Value></s:Subcode></s:Code><s:Reason><s:Text xml:lang=\"en\">bad</s:Text></s:Reason><s:Detail><svc:JobId>7</svc:JobId></s:Detail></s:Fault></s:Body></s:Envelope>",
            ns::SOAP,
            ns::WSA
        )
    }

    fn parse_one(xml_text: &str) -> Fault {
        let doc = xml::parse(xml_text).unwrap();
        let f = doc.descendants().find(|n| xml::is(*n, ns::SOAP, "Fault")).unwrap();
        parse_fault(f)
    }

    #[test]
    fn the_structure_is_read_and_subcodes_are_matched_by_namespace() {
        let f = parse_one(&fault_xml("svc:Busy", ""));
        assert_eq!(f.role, Role::Sender);
        assert_eq!((f.reason.as_str(), f.detail.as_str()), ("bad", "7"));
        assert!(f.code.is(SVC, "Busy"));
        assert!(f.to_string().contains("Busy"));
        // Another prefix for the same namespace is the same code; a look-alike namespace is not.
        assert!(parse_one(&fault_xml("q:Busy", &format!(" xmlns:q=\"{SVC}\""))).code.is(SVC, "Busy"));
        assert!(!parse_one(&fault_xml("q:Busy", " xmlns:q=\"urn:other\"")).code.is(SVC, "Busy"));
        assert!(parse_one(&fault_xml("wsa:ActionNotSupported", "")).code.is_action_not_supported());
        assert!(!parse_one(&fault_xml("svc:ActionNotSupported", "")).code.is_action_not_supported());
    }

    #[test]
    fn undeclared_prefixes_bare_faults_and_roles_degrade_quietly() {
        assert_eq!(parse_one(&fault_xml("zz:Busy", "")).code, SubCode::default());
        assert_eq!(parse_one(&fault_xml("", "")).code, SubCode::default());
        let bare = format!("<s:Envelope xmlns:s=\"{}\"><s:Body><s:Fault/></s:Body></s:Envelope>", ns::SOAP);
        let f = parse_one(&bare);
        assert_eq!((f.role, f.reason.as_str(), f.detail.as_str()), (Role::Other, "", ""));
        assert_eq!(parse_one(&fault_xml("svc:X", "").replace("s:Sender", "s:Receiver")).role, Role::Receiver);
    }
}
