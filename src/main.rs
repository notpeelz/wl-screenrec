extern crate ffmpeg_next as ffmpeg;

use std::{
    collections::VecDeque,
    ffi::c_int,
    fs::File,
    mem::MaybeUninit,
    os::{
        fd::{AsFd, AsRawFd},
        raw::c_void,
    },
    ptr::{null, null_mut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

use clap::{command, Parser};
use ffmpeg::{
    codec::{self, Parameters},
    dict, encoder,
    ffi::{
        av_buffer_ref, av_buffer_unref, av_buffersrc_parameters_alloc, av_buffersrc_parameters_set,
        av_free, av_hwdevice_ctx_create, av_hwframe_ctx_alloc, av_hwframe_ctx_create_derived,
        av_hwframe_ctx_init, av_hwframe_get_buffer, av_hwframe_map, AVBufferRef,
        AVDRMFrameDescriptor, AVFrame, AVHWDeviceContext, AVHWFramesContext, AVPixelFormat,
        AV_HWFRAME_MAP_READ, AV_HWFRAME_MAP_WRITE,
    },
    filter,
    format::{self, Pixel},
    frame::{self, video},
    Codec, Packet,
};
// use gbm::{BufferObject, BufferObjectFlags, Device};
use image::{EncodableLayout, ImageBuffer, ImageOutputFormat, Rgba};
use vaapi_sys::{
    vaCreateSurfaces, vaExportSurfaceHandle, vaGetDisplayDRM, vaInitialize,
    VADRMPRIMESurfaceDescriptor, _VADRMPRIMESurfaceDescriptor__bindgen_ty_1, vaBeginPicture,
    vaCreateConfig, vaCreateContext, vaDeriveImage, vaDestroyImage, vaEndPicture, vaMapBuffer,
    vaRenderPicture, vaUnmapBuffer, VAEntrypoint_VAEntrypointEncSlice,
    VAEntrypoint_VAEntrypointEncSliceLP, VAImage, VAProfile_VAProfileAV1Profile0,
    VAProfile_VAProfileH264High, VA_EXPORT_SURFACE_READ_WRITE, VA_FOURCC_XRGB, VA_PROGRESSIVE,
    VA_RT_FORMAT_RGB32, VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
};
use wayland_client::{
    event_enum,
    protocol::wl_output::{self, WlOutput},
    Display, Filter, GlobalManager, Interface, Main,
};
use wayland_protocols::{
    unstable::linux_dmabuf::{
        self,
        v1::client::{
            zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
            zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        },
    },
    wlr::unstable::screencopy::v1::client::{
        zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
    },
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {}

struct VaSurface {
    // va_surface: u32,
    // export: VADRMPRIMESurfaceDescriptor,
    // img: VAImage,
    f: video::Video,
}

struct State {
    dims: Option<(i32, i32)>,
    
    surfaces_owned_by_compositor: VecDeque<VaSurface>,
    surfaces_owned_by_filter: VecDeque<VaSurface>,
    free_surfaces: Vec<VaSurface>,


    // va_dpy: *mut c_void,
    // va_context: u32,
    enc: ffmpeg_next::encoder::Video,
    filter: filter::graph::Graph,
    ost_index: usize,
    octx: format::context::Output,
    dma_params: Main<ZwpLinuxBufferParamsV1>,
    copy_manager: Main<ZwlrScreencopyManagerV1>,
    wl_output: Main<WlOutput>,
    running: Arc<AtomicBool>,
}

impl State {
    fn process_ready(&mut self) {
        let mut encoded = Packet::empty();
        while self.enc.receive_packet(&mut encoded).is_ok() {
            encoded.set_stream(self.ost_index);
            // rescale?
            encoded.write_interleaved(&mut self.octx).unwrap();
        }
    }
    fn queue_copy(&mut self) {
        let surf = self.free_surfaces.pop().unwrap();

        let mut dst = video::Video::empty();
        dst.set_format(Pixel::DRM_PRIME);

        unsafe {
            let sts = av_hwframe_map(
                dst.as_mut_ptr(),
                surf.f.as_ptr(),
                AV_HWFRAME_MAP_WRITE as c_int | AV_HWFRAME_MAP_READ as c_int,
            );
            assert_eq!(sts, 0);

            let desc = &*((*dst.as_ptr()).data[0] as *const AVDRMFrameDescriptor);

            let modifier = desc.objects[0].format_modifier.to_be_bytes();
            let stride = desc.layers[0].planes[0].pitch as u32;
            let fd = desc.objects[0].fd;

            // let modifier = surf.export.objects[0].drm_format_modifier.to_be_bytes();
            // let stride = surf.export.layers[0].pitch[0];
            // let fd =                 surf.export.objects[0].fd;
            self.dma_params.add(
                fd,
                0,
                0,
                stride,
                u32::from_be_bytes(modifier[..4].try_into().unwrap()),
                u32::from_be_bytes(modifier[4..].try_into().unwrap()),
            );
        }

        let out = self.copy_manager.capture_output(1, &*self.wl_output);

        out.assign(Filter::new(
            move |(interface, event), _, mut d| match event {
                zwlr_screencopy_frame_v1::Event::Ready {
                    tv_sec_hi,
                    tv_sec_lo,
                    tv_nsec,
                } => {
                    let state = d.get::<State>().unwrap();

                    let surf = state.surfaces_owned_by_compositor.pop_front().unwrap();

                    // unsafe {
                    //     let sts = vaBeginPicture(state.va_dpy, state.va_context, surf.va_surface);
                    //     assert_eq!(sts, 0);

                    //     let sts = vaRenderPicture(state.va_dpy, state.va_context, null_mut(), 0);
                    //     assert_eq!(sts, 0);

                    //     let sts = vaEndPicture(state.va_dpy, state.va_context);
                    //     assert_eq!(sts, 0);
                    // };

                    // let mut frame = ffmpeg_next::frame::video::Video::empty();

                    // frame.set_format(Pixel::VAAPI);
                    // unsafe { (*frame.as_mut_ptr()).data[3] = surf.va_surface as usize as *mut _ };

                    // state.enc.as_mut().unwrap().send_frame(&surf.f).unwrap();
                    state
                        .filter
                        .get("in")
                        .unwrap()
                        .source()
                        .add(&surf.f)
                        .unwrap();

                    state.surfaces_owned_by_filter.push_back(surf);

                    let mut yuv_frame = frame::Video::empty();
                    while state
                        .filter
                        .get("out")
                        .unwrap()
                        .sink()
                        .frame(&mut yuv_frame)
                        .is_ok()
                    {
                        state.free_surfaces.push(state.surfaces_owned_by_filter.pop_front().unwrap());
                        state.enc.send_frame(&yuv_frame).unwrap();
                    }

                    state.process_ready();
                }
                _ => {}
            },
        ));

        let buf = self.dma_params.create_immed(
            self.dims.unwrap().0,
            self.dims.unwrap().1,
            gbm::Format::Xrgb8888 as u32,
            zwp_linux_buffer_params_v1::Flags::empty(),
        );

        // dma_params.destroy();

        out.copy_with_damage(&*buf);

        self.surfaces_owned_by_compositor.push_back(surf);
    }

    fn eof_flush(&mut self)  {
        self.filter.get("in").unwrap().source().flush().unwrap();
        self.process_ready();
        self.enc.send_eof().unwrap();
        self.process_ready();
        self.octx.write_trailer().unwrap();
    }
}

struct AvHwDevCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl AvHwDevCtx {
    fn new_libva() -> Self {
        unsafe {
            let mut hw_device_ctx = null_mut();

            let opts = dict! {
                "connection_type" => "drm"
            };

            let sts = av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                &b"/dev/dri/card0\0"[0] as *const _ as *const _,
                opts.as_mut_ptr(),
                0,
            );
            assert_eq!(sts, 0);

            Self { ptr: hw_device_ctx }
        }
    }

    // fn new_drm() -> Self {
    //     unsafe {
    //         let mut hw_device_ctx = null_mut();

    //         let opts = dict! {
    //             "connection_type" => "drm"
    //         };

    //         let sts = av_hwdevice_ctx_create(
    //             &mut hw_device_ctx,
    //             ffmpeg_next::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
    //             null_mut(),
    //             opts.as_mut_ptr(),
    //             0,
    //         );
    //         assert_eq!(sts, 0);

    //         Self {
    //             ptr: hw_device_ctx
    //         }
    //     }
    // }

    fn create_frame_ctx(&mut self, pixfmt: AVPixelFormat) -> Result<AvHwFrameCtx, ffmpeg::Error> {
        unsafe {
            let mut hwframe = av_hwframe_ctx_alloc(self.ptr as *mut _);
            let hwframe_casted = (*hwframe).data as *mut AVHWFramesContext;

            // ffmpeg does not expose RGB vaapi
            (*hwframe_casted).format = Pixel::VAAPI.into();
            // (*hwframe_casted).sw_format = AVPixelFormat::AV_PIX_FMT_YUV420P;
            (*hwframe_casted).sw_format = pixfmt;
            (*hwframe_casted).width = 3840;
            (*hwframe_casted).height = 2160;
            (*hwframe_casted).initial_pool_size = 20;

            let sts = av_hwframe_ctx_init(hwframe);
            assert_eq!(sts, 0);

            let ret = Ok(AvHwFrameCtx {
                ptr: av_buffer_ref(hwframe),
            });

            av_buffer_unref(&mut hwframe);

            ret
        }
    }
}

struct AvHwFrameCtx {
    ptr: *mut ffmpeg::sys::AVBufferRef,
}

impl AvHwFrameCtx {
    fn alloc(&mut self) -> video::Video {
        let mut frame = ffmpeg_next::frame::video::Video::empty();
        let sts = unsafe { av_hwframe_get_buffer(self.ptr, frame.as_mut_ptr(), 0) };
        assert_eq!(sts, 0);

        frame
    }
}

// unsafe fn alloc_va_surface(va_dpy: *mut c_void, w: i32, h: i32) -> VaSurface {
//     let mut surface = 0;
//     let sts = vaCreateSurfaces(
//         va_dpy,
//         VA_RT_FORMAT_RGB32,
//         w as u32,
//         h as u32,
//         &mut surface,
//         1,
//         null_mut(),
//         0,
//     );
//     if sts != 0 {
//         panic!();
//     }

//     let mut p: MaybeUninit<VADRMPRIMESurfaceDescriptor> = MaybeUninit::uninit();
//     let sts = vaExportSurfaceHandle(
//         va_dpy,
//         surface,
//         VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
//         VA_EXPORT_SURFACE_READ_WRITE,
//         p.as_mut_ptr() as *mut _,
//     );
//     if sts != 0 {
//         panic!();
//     }

//     let mut image = MaybeUninit::uninit();
//     let sts = vaDeriveImage(va_dpy, surface, image.as_mut_ptr());
//     if sts != 0 {
//         panic!();
//     }

//     VaSurface {
//         va_surface: surface,
//         export: p.assume_init(),
//         img: image.assume_init(),
//     }
// }

fn filter(inctx: &AvHwFrameCtx) -> filter::Graph {
    let mut g = ffmpeg::filter::graph::Graph::new();
    g.add(
        &filter::find("buffer").unwrap(),
        "in",
        &format!(
            "video_size=2840x2160:pix_fmt={}:time_base=1/60",
            AVPixelFormat::AV_PIX_FMT_VAAPI as c_int
        ),
    )
    .unwrap();

    unsafe {
        let p = &mut *av_buffersrc_parameters_alloc();

        p.width = 3840;
        p.height = 2161;
        p.format = AVPixelFormat::AV_PIX_FMT_VAAPI as c_int;
        p.time_base.num = 60;
        p.time_base.den = 1;
        p.hw_frames_ctx = inctx.ptr;

        let sts = av_buffersrc_parameters_set(g.get("in").unwrap().as_mut_ptr(), p as *mut _);
        assert_eq!(sts, 0);

        av_free(p as *mut _ as *mut _);
    }

    // g.add(
    //     &ffmpeg_next::filter::find("scale_vaapi").unwrap(),
    //     "pixfmt_convert",
    //     "format=nv12",
    // )
    // .unwrap();
    g.add(&filter::find("buffersink").unwrap(), "out", "")
        .unwrap();

    let mut out = g.get("out").unwrap();
    out.set_pixel_format(Pixel::VAAPI);

    // g.output("in", 0)
    //     .unwrap()
    //     .input("pixfmt_convert", 0)
    //     .unwrap();
    g.output("in", 0)
        .unwrap()
        .input("out", 0)
        .unwrap()
        .parse("scale_vaapi=format=nv12")
        .unwrap();

    // g.get("Parsed_scale_vaapi").unwrap().as_mut_ptr();

    g.validate().unwrap();

    g
}

fn main() {
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || r.store(false, Ordering::SeqCst)).unwrap();

    ffmpeg_next::init().unwrap();

    ffmpeg_next::log::set_level(ffmpeg::log::Level::Trace);

    let args = Args::parse();

    let conn = Display::connect_to_env().unwrap();
    let mut eq = conn.create_event_queue();
    let attachment = conn.attach(eq.token());

    let gm = GlobalManager::new(&attachment);

    eq.sync_roundtrip(&mut (), |_, _, _| unreachable!())
        .unwrap();

    let mut outputs = Vec::new();
    for (name, interface_name, _version) in gm.list() {
        if interface_name == wl_output::WlOutput::NAME {
            outputs.push(name);
        }
    }

    if outputs.len() != 1 {
        panic!("oops for now!");
    }
    let output = outputs[0];

    let wl_output: Main<WlOutput> = attachment.get_registry().bind(WlOutput::VERSION, output);

    // let out: Main<ZxdgOutputManagerV1> = gm.instantiate_exact(ZxdgOutputManagerV1::VERSION).unwrap();

    // out.get

    let man: Main<ZwlrScreencopyManagerV1> = gm
        .instantiate_exact(ZwlrScreencopyManagerV1::VERSION)
        .unwrap();

    let dma: Main<ZwpLinuxDmabufV1> = gm.instantiate_exact(ZwpLinuxDmabufV1::VERSION).unwrap();
    let dma_params = dma.create_params();

    // dma.assign(Filter::new(move |ev, _, _| match ev {
    //     Events::Dma { event: ev, object: o } => {

    //     }

    // }));

    wl_output.assign(Filter::new(
        move |(interface, event), _, mut d| match event {
            wl_output::Event::Mode {
                flags,
                width,
                height,
                refresh,
            } => {
                if flags.contains(wl_output::Mode::Current) {
                    d.get::<State>().unwrap().dims = Some((width, height));
                }
            }
            _ => {}
        },
    ));

    let mut output = ffmpeg_next::format::output(&"out.mp4").unwrap();
    let mut ost = output
        .add_stream(ffmpeg_next::encoder::find(codec::Id::H264))
        .unwrap();

    let mut param = Parameters::new();
    unsafe {
        (*param.as_mut_ptr()).codec_id = codec::id::Id::H264.into();
    }

    // let mut enc = codec::context::Context::from_parameters(ost.parameters()).unwrap()
    let mut enc = codec::context::Context::from_parameters(param)
        .unwrap()
        .encoder()
        .video()
        .unwrap();

    enc.set_format(Pixel::VAAPI);
    enc.set_flags(codec::Flags::GLOBAL_HEADER);
    enc.set_width(3840);
    enc.set_height(2160);
    enc.set_time_base((1, 60));

    let mut hw_device_ctx = AvHwDevCtx::new_libva();
    let mut frames_rgb = hw_device_ctx
        .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_RGB0)
        .unwrap();

    let mut frames_yuv = hw_device_ctx
        .create_frame_ctx(AVPixelFormat::AV_PIX_FMT_NV12)
        .unwrap();

    unsafe {
        (*enc.as_mut_ptr()).hw_device_ctx = hw_device_ctx.ptr as *mut _;
        (*enc.as_mut_ptr()).hw_frames_ctx = frames_yuv.ptr as *mut _;
        (*enc.as_mut_ptr()).sw_pix_fmt = AVPixelFormat::AV_PIX_FMT_NV12;
    }

    let g = filter(&frames_rgb);
    println!("{}", g.dump());

    // let mut out = g.get("out").unwrap();
    // out.set_pixel_format(Pixel::VAAPI);

    ost.set_parameters(&enc);
    let enc = enc
        .open_as_with(
            encoder::find_by_name("h264_vaapi"),
            dict! {
                // "profile" => "high"
            },
        )
        .unwrap();

    ffmpeg_next::format::context::output::dump(&output, 0, Some(&"out.mp4"));
    output.write_header().unwrap();

    let mut state = State {
        dims: None,
        free_surfaces: Vec::new(),
        // va_dpy: null_mut(),
        // va_context: 0,
        surfaces_owned_by_compositor: VecDeque::new(),
        surfaces_owned_by_filter: VecDeque::new(),
        enc,
        filter: g,
        ost_index: 0, // ??
        octx: output,
        dma_params,
        copy_manager: man,
        wl_output,
        running,
    };

    eq.sync_roundtrip(&mut state, |a, b, c| println!("{:?} {:?} {:?}", a, b, c))
        .unwrap();

    let (w, h) = state.dims.unwrap();

    // TODO: detect formats
    let mut drm_device_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/dri/card0")
        .unwrap();

    for _ in 0..1 {
        state.free_surfaces.push(VaSurface {
            f: frames_rgb.alloc(),
        });
    }

            // state.queue_copy();
    while state.running.load(Ordering::SeqCst) {
        while !state.free_surfaces.is_empty() {
            state.queue_copy();
        }
        eq.dispatch(&mut state, |_, _, _| ()).unwrap();
    }
    // loop {
    //     eq.dispatch(&mut state, |_, _, _| ()).unwrap();
    // }

    state.eof_flush();
}
