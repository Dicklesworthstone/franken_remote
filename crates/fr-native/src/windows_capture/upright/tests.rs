//! Opt-in execution on a real Windows D3D11 video processor. This checks
//! rotation direction and corners through the production GPU operation; it
//! is not a fixture standing in for native execution.
use super::{Layout, Upright};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
    IDXGIAdapter,
};

const WIDTH: u32 = 640;
const HEIGHT: u32 = 480;
const LABELS: [[u8; 4]; 4] = [
    [10, 30, 210, 255],   // top left
    [20, 190, 40, 255],   // top right
    [180, 50, 60, 255],   // bottom left
    [200, 210, 220, 255], // bottom right
];

fn device() -> (ID3D11Device, ID3D11DeviceContext) {
    let (mut device, mut context) = (None, None);
    // SAFETY: live out pointers, default hardware adapter, no software DLL.
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_VIDEO_SUPPORT | D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&raw mut device),
            None,
            Some(&raw mut context),
        )
    }
    .expect("a hardware D3D11 device");
    (device.unwrap(), context.unwrap())
}

fn texture(
    device: &ID3D11Device,
    size: (u32, u32),
    pixels: Option<&[u8]>,
    readback: bool,
) -> ID3D11Texture2D {
    let description = D3D11_TEXTURE2D_DESC {
        Width: size.0,
        Height: size.1,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: if readback {
            D3D11_USAGE_STAGING
        } else {
            D3D11_USAGE_DEFAULT
        },
        BindFlags: if readback {
            0
        } else {
            (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0).cast_unsigned()
        },
        CPUAccessFlags: if readback {
            D3D11_CPU_ACCESS_READ.0.cast_unsigned()
        } else {
            0
        },
        MiscFlags: 0,
    };
    let data = pixels.map(|pixels| {
        assert_eq!(pixels.len(), (size.0 * size.1 * 4) as usize);
        D3D11_SUBRESOURCE_DATA {
            pSysMem: pixels.as_ptr().cast(),
            SysMemPitch: size.0 * 4,
            SysMemSlicePitch: 0,
        }
    });
    let mut result = None;
    // SAFETY: bounded test dimensions, complete optional initial data and a
    // live device. The API copies the initial bytes before returning.
    unsafe {
        device.CreateTexture2D(
            &raw const description,
            data.as_ref().map(core::ptr::from_ref),
            Some(&raw mut result),
        )
    }
    .expect("bounded BGRA test texture");
    result.unwrap()
}

struct Readback<'a> {
    context: &'a ID3D11DeviceContext,
    texture: &'a ID3D11Texture2D,
    data: D3D11_MAPPED_SUBRESOURCE,
}

impl Drop for Readback<'_> {
    fn drop(&mut self) {
        // SAFETY: the one successful Map below, released exactly once even
        // if a validation assertion fails while inspecting the result.
        unsafe { self.context.Unmap(self.texture, 0) };
    }
}

fn corners(
    context: &ID3D11DeviceContext,
    image: &ID3D11Texture2D,
    size: (u32, u32),
) -> [[u8; 4]; 4] {
    let mut data = D3D11_MAPPED_SUBRESOURCE::default();
    // SAFETY: a live staging texture, a valid out pointer; blocking read Map
    // waits for the preceding GPU copy before exposing its bytes.
    unsafe { context.Map(image, 0, D3D11_MAP_READ, 0, Some(&raw mut data)) }
        .expect("GPU rotation readback");
    let readback = Readback {
        context,
        texture: image,
        data,
    };
    assert!(!readback.data.pData.is_null());
    assert!(readback.data.RowPitch >= size.0 * 4);
    [
        (0, 0),
        (size.0 - 1, 0),
        (0, size.1 - 1),
        (size.0 - 1, size.1 - 1),
    ]
    .map(|(x, y)| {
        let offset =
            usize::try_from(u64::from(y) * u64::from(readback.data.RowPitch) + u64::from(x) * 4)
                .expect("mapped test pixel offset");
        // SAFETY: all four coordinates lie within the mapped BGRA texture,
        // row pitch holds the full width, and Readback keeps the map alive.
        unsafe {
            let pixel = readback.data.pData.cast::<u8>().add(offset);
            [*pixel, *pixel.add(1), *pixel.add(2), *pixel.add(3)]
        }
    })
}

#[test]
#[ignore = "requires a real Windows D3D11 video processor with BGRA rotation"]
fn rotates_labeled_corners_on_hardware() {
    let (device, context) = device();
    let mut pixels = Vec::with_capacity((WIDTH * HEIGHT * 4) as usize);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let label = usize::from(y >= HEIGHT / 2) * 2 + usize::from(x >= WIDTH / 2);
            pixels.extend_from_slice(&LABELS[label]);
        }
    }
    let source = texture(&device, (WIDTH, HEIGHT), Some(&pixels), false);
    for (degrees, size, expected) in [
        (
            90,
            (HEIGHT, WIDTH),
            [LABELS[2], LABELS[0], LABELS[3], LABELS[1]],
        ),
        (
            180,
            (WIDTH, HEIGHT),
            [LABELS[3], LABELS[2], LABELS[1], LABELS[0]],
        ),
        (
            270,
            (HEIGHT, WIDTH),
            [LABELS[1], LABELS[3], LABELS[0], LABELS[2]],
        ),
    ] {
        let destination = texture(&device, size, None, false);
        let readback = texture(&device, size, None, true);
        let layout = Layout::new(size, degrees, WIDTH, u64::from(WIDTH * HEIGHT)).unwrap();
        let upright = Upright::open(&device, &destination, layout, 60)
            .expect("hardware BGRA rotation without temporal surfaces");
        upright.write(&source).expect("rotation submission");
        // SAFETY: equal-sized BGRA textures on one device; the immediate
        // context orders this copy after the production rotation operation.
        unsafe { context.CopyResource(&readback, &destination) };
        assert_eq!(
            corners(&context, &readback, size),
            expected,
            "clockwise {degrees}"
        );
    }
}
