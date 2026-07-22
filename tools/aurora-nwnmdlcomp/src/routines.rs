use anyhow::{Context, Result, bail};
use nwnrs_types::mdl::{BinaryModel, BinaryNode, NodeKind};

const FILE_HEADER_SIZE: usize = 12;
const NODE_HEADER_SIZE: usize = 112;

const MODEL_ROUTINES: [u32; 2] = [0x0046_ab0c, 0x0046_ab1c];
const ANIMATION_ROUTINES: [u32; 2] = [0x0046_a144, 0x004c_64e4];

const DUMMY_ROUTINES: [u32; 6] = [
    0x0046_b9e0,
    0x0046_b9f0,
    0x0046_ba04,
    0x0046_ba14,
    0x0046_ba30,
    0x0046_ba40,
];
const LIGHT_ROUTINES: [u32; 6] = [
    0x0046_dbb8,
    0x0046_dbc8,
    0x0046_dbdc,
    0x0046_dbec,
    0x0046_dc08,
    0x0046_dc18,
];
const EMITTER_ROUTINES: [u32; 6] = [
    0x0046_dbb8,
    0x0046_da00,
    0x0046_da14,
    0x0046_da24,
    0x0046_da40,
    0x0046_da50,
];
const CAMERA_ROUTINES: [u32; 6] = [
    0x0046_b9e0,
    0x0046_d260,
    0x0046_ba04,
    0x0046_d274,
    0x0046_d290,
    0x0046_d2a0,
];
const REFERENCE_ROUTINES: [u32; 6] = [
    0x0046_b9e0,
    0x0046_e178,
    0x0046_ba04,
    0x0046_e18c,
    0x0046_e1a8,
    0x0046_e1b8,
];
const TRIMESH_ROUTINES: [u32; 6] = [
    0x0046_e7b4,
    0x0046_e7c4,
    0x0046_e7d8,
    0x0046_e7e8,
    0x0046_e804,
    0x0046_e814,
];
const SKIN_ROUTINES: [u32; 6] = [
    0x0046_d394,
    0x0046_d3a4,
    0x0046_d3b8,
    0x0046_e7e8,
    0x0046_d3c8,
    0x0046_d3d8,
];
const ANIMMESH_ROUTINES: [u32; 6] = [
    0x0046_cf88,
    0x0046_cf98,
    0x0046_cfac,
    0x0046_cfbc,
    0x0046_cfd8,
    0x0046_cfe8,
];
const DANGLY_ROUTINES: [u32; 6] = [
    0x0046_e2b4,
    0x0046_e2c4,
    0x0046_e2d8,
    0x0046_e7e8,
    0x0046_e804,
    0x0046_e2e8,
];
const AABB_ROUTINES: [u32; 6] = [
    0x0046_ed74,
    0x0046_ed84,
    0x0046_e7d8,
    0x0046_e7e8,
    0x0046_e804,
    0x0046_ed98,
];

const TRIMESH_MESH_ROUTINES: [u32; 2] = [0x0046_e828, 0x0046_e838];
const SKIN_MESH_ROUTINES: [u32; 2] = [0x0046_d3ec, 0x0046_d3fc];
const ANIMMESH_MESH_ROUTINES: [u32; 2] = [0x0046_cffc, 0x0046_d00c];
const DANGLY_MESH_ROUTINES: [u32; 2] = [0x0046_e2fc, 0x0046_e30c];

pub(crate) fn patch_bioware_routines(bytes: &mut [u8], model: &BinaryModel) -> Result<()> {
    patch_words(bytes, FILE_HEADER_SIZE, &MODEL_ROUTINES)?;
    for node in model.nodes() {
        patch_node(bytes, node)?;
    }
    for animation in model.animations() {
        let animation_offset = model_offset(animation.offset)?;
        patch_words(bytes, animation_offset, &ANIMATION_ROUTINES)?;
        for node in &animation.nodes {
            patch_node(bytes, node)?;
        }
    }
    Ok(())
}

fn patch_node(bytes: &mut [u8], node: &BinaryNode) -> Result<()> {
    let node_offset = model_offset(node.offset)?;
    let routines = match &node.kind {
        NodeKind::Dummy => &DUMMY_ROUTINES,
        NodeKind::Light => &LIGHT_ROUTINES,
        NodeKind::Emitter => &EMITTER_ROUTINES,
        NodeKind::Camera => &CAMERA_ROUTINES,
        NodeKind::Reference => &REFERENCE_ROUTINES,
        NodeKind::Trimesh | NodeKind::Patch => &TRIMESH_ROUTINES,
        NodeKind::Skin => &SKIN_ROUTINES,
        NodeKind::Animmesh => &ANIMMESH_ROUTINES,
        NodeKind::Danglymesh => &DANGLY_ROUTINES,
        NodeKind::Aabb => &AABB_ROUTINES,
        NodeKind::Other(kind) => bail!("cannot emit routine table for unknown node type {kind}"),
    };
    patch_words(bytes, node_offset, routines)?;

    if node.content.has_mesh {
        let mesh_routines = match &node.kind {
            NodeKind::Skin => &SKIN_MESH_ROUTINES,
            NodeKind::Animmesh => &ANIMMESH_MESH_ROUTINES,
            NodeKind::Danglymesh => &DANGLY_MESH_ROUTINES,
            NodeKind::Trimesh | NodeKind::Aabb | NodeKind::Patch => &TRIMESH_MESH_ROUTINES,
            other => bail!("node type {other:?} carries an unexpected mesh header"),
        };
        patch_words(bytes, node_offset + NODE_HEADER_SIZE, mesh_routines)?;
    }
    Ok(())
}

fn model_offset(offset: u32) -> Result<usize> {
    usize::try_from(offset)
        .context("model offset does not fit this platform")?
        .checked_add(FILE_HEADER_SIZE)
        .context("model offset overflow")
}

fn patch_words(bytes: &mut [u8], offset: usize, words: &[u32]) -> Result<()> {
    let byte_len = words
        .len()
        .checked_mul(4)
        .context("routine table overflow")?;
    let end = offset
        .checked_add(byte_len)
        .context("routine offset overflow")?;
    let target = bytes
        .get_mut(offset..end)
        .context("routine table lies outside compiled model data")?;
    for (chunk, word) in target.chunks_exact_mut(4).zip(words) {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    Ok(())
}
