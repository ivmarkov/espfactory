use std::io::Write;

pub mod dir;
pub mod s3;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum BundleType {
    Complete,
    BinAppImage,
    ElfAppImage,
}

impl BundleType {
    pub fn iter() -> impl Iterator<Item = Self> {
        [Self::Complete, Self::BinAppImage, Self::ElfAppImage].into_iter()
    }

    pub fn file(&self, id: &str) -> String {
        format!("{}{}", id, self.suffix())
    }

    pub const fn suffix(&self) -> &str {
        match self {
            Self::Complete => ".bundle",
            Self::BinAppImage => ".bin",
            Self::ElfAppImage => "",
        }
    }
}

pub trait BundleLoader {
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write;
}

impl<T> BundleLoader for &mut T
where
    T: BundleLoader,
{
    async fn load<W>(&mut self, write: W, id: Option<&str>) -> anyhow::Result<String>
    where
        W: Write,
    {
        (*self).load(write, id).await
    }
}
