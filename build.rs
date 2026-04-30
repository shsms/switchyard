// Until switchyard has its own proto submodule, reuse the one already
// vendored under ../microsim. The override env var lets a downstream
// packager point at a private mirror without editing build.rs.
use std::path::PathBuf;

fn main() -> Result<(), std::io::Error> {
    let proto_root = std::env::var("SWITCHYARD_PROTO_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("../microsim/submodules/frequenz-api-microgrid")
        });

    let microgrid_proto = proto_root
        .join("proto/frequenz/api/microgrid/v1alpha18/microgrid.proto");
    let common_proto_root = proto_root.join("submodules/frequenz-api-common/proto");
    let google_proto_root = proto_root.join("submodules/api-common-protos");

    println!("cargo:rerun-if-env-changed=SWITCHYARD_PROTO_ROOT");
    println!("cargo:rerun-if-changed={}", microgrid_proto.display());

    tonic_prost_build::configure()
        .disable_comments(["."])
        .include_file("proto_v1_alpha18.rs")
        .compile_well_known_types(false)
        .compile_protos(
            &[microgrid_proto.as_path()],
            &[
                proto_root.join("proto").as_path(),
                common_proto_root.as_path(),
                google_proto_root.as_path(),
            ],
        )
        .inspect_err(|e| {
            eprintln!("Could not compile protobuf files. Error: {:?}", e);
        })
}
