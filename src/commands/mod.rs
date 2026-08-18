pub mod query;
pub mod recover;
pub mod rollback;
pub mod run;
pub mod scan_leftovers;
pub mod simulate;
pub mod track;

pub(crate) fn is_root() -> bool {
    unsafe { libc::geteuid() == 0 }
}
