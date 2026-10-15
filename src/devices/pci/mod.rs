pub mod config;
pub mod msix;
pub mod root;

use config::BarChange;

pub trait PciDevice: Send {
    fn read_config(&mut self, offset: usize, data: &mut [u8]);
    fn write_config(&mut self, offset: usize, data: &[u8]) -> Option<BarChange>;
}
