use crate::aml::{
    AmlError,
    Operation,
    namespace::AmlName,
    object::Object,
};
use alloc::vec::Vec;
use bit_field::BitField;
use core::str::FromStr;

pub use crate::aml::resource::IrqDescriptor;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Pin {
    IntA,
    IntB,
    IntC,
    IntD,
}

/// The `_PRT` source field of one routing entry.
///
/// This is a decoded firmware fact, not a routing decision. In particular, a
/// link source keeps its `SourceIndex`: whether index zero is required is the
/// consumer's policy, not part of the `_PRT` format.
#[derive(Debug, PartialEq, Clone)]
pub enum PciRouteSource {
    /// The interrupt is hard-coded to a GSI.
    Gsi(u32),

    /// The interrupt is linked to a link object with `_PRS`, `_CRS` and `_SRS`.
    /// `name` is the decoded AML name path; `index` is the `SourceIndex` field.
    Link { name: AmlName, index: u32 },
}

/// Why a `_PRT` entry's source fields could not be decoded.
///
/// The `(device, function, pin)` key of such an entry is still valid, so the
/// table retains it with this error and lets the consumer decide how to handle
/// the unusable route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PciRouteSourceError {
    /// The source is neither the integer zero nor a name string, or its
    /// `SourceIndex` element is not an integer.
    MalformedSource,
    /// A direct GSI does not fit the 32-bit `SourceIndex` field.
    InvalidGsi,
    /// A link's `SourceIndex` does not fit the 32-bit field.
    InvalidLinkIndex,
    /// The link name string is not a valid AML name.
    InvalidLinkName,
}

#[derive(Debug, PartialEq, Clone)]
pub struct PciRoutingEntry {
    device: u16,
    function: u16,
    pin: Pin,
    source: Result<PciRouteSource, PciRouteSourceError>,
}

impl PciRoutingEntry {
    /// The PCI device number (the high word of the `_PRT` address).
    pub fn device(&self) -> u16 {
        self.device
    }

    /// The PCI function number (the low word of the `_PRT` address).
    /// `0xffff` is the wildcard function.
    pub fn function(&self) -> u16 {
        self.function
    }

    /// The one-based PCI interrupt pin.
    pub fn pin(&self) -> Pin {
        self.pin
    }

    /// The decoded source, or the reason its source fields are malformed.
    pub fn source(&self) -> Result<&PciRouteSource, PciRouteSourceError> {
        self.source.as_ref().map_err(|error| *error)
    }
}

/// A `PciRoutingTable` is the decoded content of a `_PRT` object, which provides a mapping
/// from PCI interrupt pins to the inputs of the interrupt controller. One of these objects must be
/// present under each PCI root bridge, and consists of a package of packages, each of which describes the
/// mapping of a single PCI interrupt pin.
#[derive(Debug, PartialEq)]
pub struct PciRoutingTable {
    entries: Vec<PciRoutingEntry>,
}

impl PciRoutingTable {
    /// Decodes a `_PRT` object without evaluating AML or touching the namespace.
    ///
    /// Structural errors reject the whole table: the value must be a package,
    /// each element must be a four-element package, the address must be a
    /// 32-bit integer, and the pin must be in `0..=3`. A well-formed key whose
    /// source fields are malformed is retained with
    /// [`PciRoutingEntry::source`] returning the corresponding error.
    pub fn from_object(prt: &Object) -> Result<PciRoutingTable, AmlError> {
        let Object::Package(inner_values) = prt else {
            return Err(AmlError::InvalidOperationOnObject { op: Operation::DecodePrt, typ: prt.typ() });
        };

        let mut entries = Vec::with_capacity(inner_values.len());
        for value in inner_values {
            /*
             * Each inner package has the following structure:
             *   | Field      | Type      | Description                                               |
             *   | -----------|-----------|-----------------------------------------------------------|
             *   | Address    | Dword     | Address of the device. Same format as _ADR objects (high  |
             *   |            |           | word = #device, low word = #function)                     |
             *   | ---------- |-----------|-----------------------------------------------------------|
             *   | Pin        | Byte      | The PCI pin (0 = INTA, 1 = INTB, 2 = INTC, 3 = INTD)      |
             *   | -----------|-----------|-----------------------------------------------------------|
             *   | Source     | Byte or   | Name of the device that allocates the interrupt to which  |
             *   |            | NamePath  | the above pin is connected. Can be fully qualified,       |
             *   |            |           | relative, or a simple NameSeg that utilizes namespace     |
             *   |            |           | search rules. Instead, if this is a byte value of 0, the  |
             *   |            |           | interrupt is allocated out of the GSI pool, and Source    |
             *   |            |           | Index should be utilised.                                 |
             *   | -----------|-----------|-----------------------------------------------------------|
             *   | Source     | Dword     | Index that indicates which resource descriptor in the     |
             *   | Index      |           | resource template of the device pointed to in the Source  |
             *   |            |           | field this interrupt is allocated from. If the Source     |
             *   |            |           | is zero, then this field is the GSI number to which the   |
             *   |            |           | pin is connected.                                         |
             *   | -----------|-----------|-----------------------------------------------------------|
             */
            let Object::Package(pin_package) = &**value else {
                return Err(AmlError::InvalidOperationOnObject { op: Operation::DecodePrt, typ: value.typ() });
            };
            if pin_package.len() != 4 {
                return Err(AmlError::PrtMalformedEntry);
            }

            let Object::Integer(address) = *pin_package[0] else {
                return Err(AmlError::PrtInvalidAddress);
            };
            let address = u32::try_from(address).map_err(|_| AmlError::PrtInvalidAddress)?;
            let device = address.get_bits(16..32).try_into().map_err(|_| AmlError::PrtInvalidAddress)?;
            let function = address.get_bits(0..16).try_into().map_err(|_| AmlError::PrtInvalidAddress)?;
            let pin = match *pin_package[1] {
                Object::Integer(0) => Pin::IntA,
                Object::Integer(1) => Pin::IntB,
                Object::Integer(2) => Pin::IntC,
                Object::Integer(3) => Pin::IntD,
                _ => return Err(AmlError::PrtInvalidPin),
            };

            let source = decode_source(&pin_package[2], &pin_package[3]);
            entries.push(PciRoutingEntry { device, function, pin, source });
        }

        Ok(PciRoutingTable { entries })
    }

    /// The decoded entries, in firmware order.
    pub fn entries(&self) -> &[PciRoutingEntry] {
        &self.entries
    }
}

fn decode_source(source: &Object, source_index: &Object) -> Result<PciRouteSource, PciRouteSourceError> {
    match source {
        Object::Integer(0) => {
            let Object::Integer(index) = source_index else {
                return Err(PciRouteSourceError::MalformedSource);
            };
            let gsi = u32::try_from(*index).map_err(|_| PciRouteSourceError::InvalidGsi)?;
            Ok(PciRouteSource::Gsi(gsi))
        }
        Object::String(name) => {
            let Object::Integer(index) = source_index else {
                return Err(PciRouteSourceError::MalformedSource);
            };
            let index = u32::try_from(*index).map_err(|_| PciRouteSourceError::InvalidLinkIndex)?;
            let name = AmlName::from_str(name).map_err(|_| PciRouteSourceError::InvalidLinkName)?;
            Ok(PciRouteSource::Link { name, index })
        }
        _ => Err(PciRouteSourceError::MalformedSource),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{string::ToString, vec, vec::Vec};
    use crate::aml::object::WrappedObject;

    fn integer(value: u64) -> WrappedObject {
        Object::Integer(value).wrap()
    }

    fn string(value: &str) -> WrappedObject {
        Object::String(value.to_string()).wrap()
    }

    fn entry(address: u64, pin: u64, source: WrappedObject, index: WrappedObject) -> WrappedObject {
        Object::Package(vec![integer(address), integer(pin), source, index]).wrap()
    }

    fn table(entries: Vec<WrappedObject>) -> Result<PciRoutingTable, AmlError> {
        PciRoutingTable::from_object(&Object::Package(entries))
    }

    #[test]
    fn test_direct_gsi_entry() {
        let table = table(vec![entry(0x0002_0001, 0, integer(0), integer(16))]).unwrap();
        let entry = &table.entries()[0];
        assert_eq!(entry.device(), 2);
        assert_eq!(entry.function(), 1);
        assert_eq!(entry.pin(), Pin::IntA);
        assert_eq!(entry.source(), Ok(&PciRouteSource::Gsi(16)));
    }

    #[test]
    fn test_link_entry_retains_source_index() {
        let table = table(vec![entry(0x0001_ffff, 3, string("\\_SB.PCI0.LNKA"), integer(2))]).unwrap();
        let entry = &table.entries()[0];
        assert_eq!(entry.pin(), Pin::IntD);
        assert!(matches!(
            entry.source(),
            Ok(PciRouteSource::Link { index: 2, .. })
        ));
    }

    #[test]
    fn test_invalid_gsi_is_an_entry_error() {
        let table = table(vec![entry(0x0001_0000, 0, integer(0), integer(u64::from(u32::MAX) + 1))]).unwrap();
        assert_eq!(table.entries()[0].source(), Err(PciRouteSourceError::InvalidGsi));
    }

    #[test]
    fn test_invalid_link_name_is_an_entry_error() {
        let table = table(vec![entry(0x0001_0000, 0, string("TOOLONG"), integer(0))]).unwrap();
        assert_eq!(table.entries()[0].source(), Err(PciRouteSourceError::InvalidLinkName));
    }

    #[test]
    fn test_malformed_source_is_an_entry_error() {
        let table = table(vec![entry(0x0001_0000, 0, integer(1), integer(0))]).unwrap();
        assert_eq!(table.entries()[0].source(), Err(PciRouteSourceError::MalformedSource));
    }

    #[test]
    fn test_structural_errors_reject_the_whole_table() {
        assert_eq!(
            table(vec![Object::Integer(1).wrap()]),
            Err(AmlError::InvalidOperationOnObject { op: Operation::DecodePrt, typ: crate::aml::object::ObjectType::Integer })
        );
        assert_eq!(
            table(vec![Object::Package(vec![integer(0), integer(0)]).wrap()]),
            Err(AmlError::PrtMalformedEntry)
        );
        assert_eq!(
            table(vec![entry(0x0001_0000, 4, integer(0), integer(0))]),
            Err(AmlError::PrtInvalidPin)
        );
        assert_eq!(
            table(vec![entry(0x1_0000_0000, 0, integer(0), integer(0))]),
            Err(AmlError::PrtInvalidAddress)
        );
    }
}
