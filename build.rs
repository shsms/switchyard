// Both proto APIs are vendored under switchyard's own `submodules/`.
// The override env var lets a downstream packager point at a private
// mirror of the microgrid proto without editing build.rs.
use std::path::PathBuf;

fn main() -> Result<(), std::io::Error> {
    let microgrid_root = std::env::var("SWITCHYARD_PROTO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("submodules/frequenz-api-microgrid"));
    let assets_root = PathBuf::from("submodules/frequenz-api-assets");
    let dispatch_root = PathBuf::from("submodules/frequenz-api-dispatch");

    let microgrid_proto =
        microgrid_root.join("proto/frequenz/api/microgrid/v1alpha18/microgrid.proto");
    let assets_proto = assets_root.join("proto/frequenz/api/assets/v1/assets.proto");
    // dispatch v1 imports frequenz-api-common v1alpha8 (electrical_components,
    // pagination, streaming/event, types/interval) — the same common root the
    // microgrid proto pulls in, so it needs no vendored common of its own.
    let dispatch_proto = dispatch_root.join("proto/frequenz/api/dispatch/v1/dispatch.proto");
    // Both microgrid v1alpha18 and assets v1 import the same
    // frequenz-api-common (v0.8.0); we pick microgrid's vendored copy.
    let common_proto_root = microgrid_root.join("submodules/frequenz-api-common/proto");
    let google_proto_root = microgrid_root.join("submodules/api-common-protos");

    println!("cargo:rerun-if-env-changed=SWITCHYARD_PROTO_ROOT");
    println!("cargo:rerun-if-changed={}", microgrid_proto.display());
    println!("cargo:rerun-if-changed={}", assets_proto.display());
    println!("cargo:rerun-if-changed={}", dispatch_proto.display());

    tonic_prost_build::configure()
        .disable_comments(["."])
        .include_file("proto_v1_alpha18.rs")
        .compile_well_known_types(false)
        .compile_protos(
            &[
                microgrid_proto.as_path(),
                assets_proto.as_path(),
                dispatch_proto.as_path(),
            ],
            &[
                microgrid_root.join("proto").as_path(),
                assets_root.join("proto").as_path(),
                dispatch_root.join("proto").as_path(),
                common_proto_root.as_path(),
                google_proto_root.as_path(),
            ],
        )
        .inspect_err(|e| {
            eprintln!("Could not compile protobuf files. Error: {:?}", e);
        })
}
