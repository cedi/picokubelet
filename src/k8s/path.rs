use core::fmt::{self, Write as FmtWrite};

use heapless::String as HString;

pub struct ApiPath<const N: usize>(HString<N>);

impl<const N: usize> ApiPath<N> {
    pub fn new(args: fmt::Arguments<'_>) -> Result<Self, fmt::Error> {
        let mut path = HString::new();
        path.write_fmt(args)?;
        Ok(Self(path))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl<const N: usize> Clone for ApiPath<N> {
    fn clone(&self) -> Self {
        let mut path = HString::new();
        path.push_str(self.as_str()).unwrap();
        Self(path)
    }
}

#[cfg(test)]
mod tests {
    use super::ApiPath;

    #[test]
    fn formats_path_without_allocating() {
        let path = ApiPath::<64>::new(format_args!("/api/v1/nodes/{}/status", "heinrich"))
            .expect("path fits");

        assert_eq!(path.as_str(), "/api/v1/nodes/heinrich/status");
    }

    #[test]
    fn rejects_paths_that_exceed_capacity() {
        let path = ApiPath::<8>::new(format_args!("/api/v1/nodes/{}", "way-too-much-node"));

        assert!(path.is_err());
    }

    #[test]
    fn clone_preserves_path_contents() {
        let path = ApiPath::<64>::new(format_args!("/api/v1/nodes/{}", "brigitte"))
            .expect("path fits");

        assert_eq!(path.clone().as_str(), path.as_str());
    }
}
