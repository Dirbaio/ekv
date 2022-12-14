#![macro_use]

#[macro_export]
macro_rules! impl_bytes {
    ($t:ident) => {
        impl $t {
            #[allow(unused)]
            pub(crate) const SIZE: usize = core::mem::size_of::<Self>();

            #[allow(unused)]
            #[inline(always)]
            pub(crate) fn to_bytes(&self) -> [u8; Self::SIZE] {
                unsafe { core::mem::transmute(*self) }
            }

            #[allow(unused)]
            #[inline(always)]
            pub(crate) fn from_bytes(bytes: [u8; Self::SIZE]) -> Self {
                unsafe { core::mem::transmute(bytes) }
            }
        }
    };
}
