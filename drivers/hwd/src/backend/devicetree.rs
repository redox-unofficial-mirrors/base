use std::error::Error;
use std::fmt::Write;
use std::fs;

use fdt::properties::Compatible;

use super::Backend;

type FdtParser<'a> = (
    fdt::parsing::unaligned::UnalignedParser<'a>,
    fdt::parsing::Panic,
);
type Fdt<'a> = fdt::Fdt<'a, FdtParser<'a>>;
type FdtNode<'a> = fdt::nodes::Node<'a, FdtParser<'a>>;

pub struct DeviceTreeBackend {
    dtb: Vec<u8>,
}

impl DeviceTreeBackend {
    fn dump(node: FdtNode<'_>, level: usize) {
        let mut line = String::new();
        for _ in 0..level {
            line.push_str("  ");
        }
        write!(line, "{}", node.name()).unwrap();
        if let Some(compatible) = node.property::<Compatible>() {
            line.push_str(":");
            for id in compatible.all() {
                line.push_str(" ");
                line.push_str(id);
            }
        }
        log::debug!("{}", line);
        for child in node.children() {
            Self::dump(child, level + 1);
        }
    }
}

impl Backend for DeviceTreeBackend {
    fn new() -> Result<Self, Box<dyn Error>> {
        let dtb = fs::read("/scheme/kernel.dtb")?;
        // FIXME: cache this parsed result for probing?
        let _dt =
            Fdt::new_unaligned(&dtb).map_err(|err| format!("failed to parse dtb: {}", err))?;
        Ok(Self { dtb })
    }

    fn probe(&mut self) -> Result<(), Box<dyn Error>> {
        let dt =
            Fdt::new_unaligned(&self.dtb).map_err(|err| format!("failed to parse dtb: {}", err))?;
        let root = dt
            .find_node("/")
            .ok_or_else(|| format!("failed to find root node"))?;
        Self::dump(root, 0);
        Ok(())
    }
}
