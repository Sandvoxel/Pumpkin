pub mod java_packets {
    pub use pumpkin_wasm_host_packets::java_packets::*;
    use wasmtime::component::{HasData, Linker, LinkerInstance};

    pub trait Host {}
    impl<T: Host + ?Sized> Host for &mut T {}
    pub trait HostWithStore<T>: HasData {}
    impl<H: HasData + ?Sized, T> HostWithStore<T> for H {}

    pub fn add_to_linker_instance<T: 'static, D: HostWithStore<T>>(
        _instance: &mut LinkerInstance<'_, T>,
        _host_getter: fn(&mut T) -> D::Data<'_>,
    ) -> wasmtime::Result<()>
    where
        for<'a> D::Data<'a>: Host,
    {
        Ok(())
    }

    pub fn add_to_linker<T: 'static, D: HostWithStore<T>>(
        linker: &mut Linker<T>,
        host_getter: fn(&mut T) -> D::Data<'_>,
    ) -> wasmtime::Result<()>
    where
        for<'a> D::Data<'a>: Host,
    {
        let mut instance = linker.instance("pumpkin:plugin/java-packets@0.1.0")?;
        add_to_linker_instance::<T, D>(&mut instance, host_getter)
    }
}

pub mod bedrock_packets {
    pub use pumpkin_wasm_host_packets::bedrock_packets::*;
    use wasmtime::component::{HasData, Linker, LinkerInstance};

    pub trait Host {}
    impl<T: Host + ?Sized> Host for &mut T {}
    pub trait HostWithStore<T>: HasData {}
    impl<H: HasData + ?Sized, T> HostWithStore<T> for H {}

    pub fn add_to_linker_instance<T: 'static, D: HostWithStore<T>>(
        _instance: &mut LinkerInstance<'_, T>,
        _host_getter: fn(&mut T) -> D::Data<'_>,
    ) -> wasmtime::Result<()>
    where
        for<'a> D::Data<'a>: Host,
    {
        Ok(())
    }

    pub fn add_to_linker<T: 'static, D: HostWithStore<T>>(
        linker: &mut Linker<T>,
        host_getter: fn(&mut T) -> D::Data<'_>,
    ) -> wasmtime::Result<()>
    where
        for<'a> D::Data<'a>: Host,
    {
        let mut instance = linker.instance("pumpkin:plugin/bedrock-packets@0.1.0")?;
        add_to_linker_instance::<T, D>(&mut instance, host_getter)
    }
}
