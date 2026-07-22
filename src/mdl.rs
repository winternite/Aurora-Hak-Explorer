//! Aurora MDL parsing, static geometry preview, and ASCII decompilation.
//!
//! This is a bounds-checked Rust implementation and is independent of the
//! bundled compiler.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

const HEADER_SIZE: usize = 12;
const MODEL_HEADER_SIZE: usize = 0x00e8;
const NODE_HEADER_SIZE: usize = 0x0070;
const MESH_HEADER_SIZE: usize = 0x0270;
const MAX_NODES: usize = 100_000;
const MAX_VERTICES: usize = 4_000_000;
const MAX_FACES: usize = 4_000_000;
const MAX_CONTROLLER_KEYS: usize = 4_000_000;
const HAS_MESH: u32 = 0x20;

#[derive(Clone, Debug)]
pub struct Scene {
    pub name: String,
    pub meshes: Vec<SceneMesh>,
    pub vertex_count: usize,
    pub face_count: usize,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
}

#[derive(Clone, Debug)]
pub struct SceneMesh {
    pub vertices: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub faces: Vec<[u32; 3]>,
    pub texture_vertices: Vec<[f32; 2]>,
    pub texture_faces: Vec<[u32; 3]>,
    pub texture_name: Option<String>,
    pub color: [f32; 3],
}

#[derive(Clone, Debug)]
struct Document {
    name: String,
    supermodel: String,
    classification: Option<&'static str>,
    animation_scale: f32,
    ignore_fog: bool,
    nodes: Vec<Node>,
}

#[derive(Clone, Debug)]
struct Node {
    kind: &'static str,
    name: String,
    parent: Option<String>,
    position: [f32; 3],
    orientation: [f32; 4],
    scale: f32,
    ambient: [f32; 3],
    diffuse: [f32; 3],
    specular: [f32; 3],
    shininess: f32,
    shadow: u32,
    render: u32,
    bitmap: Option<String>,
    material_name: Option<String>,
    vertices: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    tverts: Vec<[f32; 2]>,
    faces: Vec<Face>,
}

#[derive(Clone, Copy, Debug)]
struct Face {
    vertices: [u32; 3],
    texture_vertices: [u32; 3],
    surface: u32,
}

#[derive(Clone, Copy, Debug)]
struct Transform {
    matrix: [[f32; 3]; 3],
    translation: [f32; 3],
}

impl Transform {
    const IDENTITY: Self = Self {
        matrix: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        translation: [0.0; 3],
    };

    fn from_node(node: &Node) -> Self {
        let [x, y, z, w] = normalize_quaternion(node.orientation);
        let scale = if node.scale.is_finite() && node.scale.abs() > 1.0e-6 {
            node.scale
        } else {
            1.0
        };
        let matrix = [
            [
                (1.0 - 2.0 * (y * y + z * z)) * scale,
                (2.0 * (x * y - z * w)) * scale,
                (2.0 * (x * z + y * w)) * scale,
            ],
            [
                (2.0 * (x * y + z * w)) * scale,
                (1.0 - 2.0 * (x * x + z * z)) * scale,
                (2.0 * (y * z - x * w)) * scale,
            ],
            [
                (2.0 * (x * z - y * w)) * scale,
                (2.0 * (y * z + x * w)) * scale,
                (1.0 - 2.0 * (x * x + y * y)) * scale,
            ],
        ];
        Self {
            matrix,
            translation: node.position,
        }
    }

    fn compose(self, local: Self) -> Self {
        let mut matrix = [[0.0; 3]; 3];
        for (row, values) in matrix.iter_mut().enumerate() {
            for (column, value) in values.iter_mut().enumerate() {
                *value = (0..3)
                    .map(|index| self.matrix[row][index] * local.matrix[index][column])
                    .sum();
            }
        }
        let moved = self.apply_direction(local.translation);
        Self {
            matrix,
            translation: [
                moved[0] + self.translation[0],
                moved[1] + self.translation[1],
                moved[2] + self.translation[2],
            ],
        }
    }

    fn apply_direction(self, point: [f32; 3]) -> [f32; 3] {
        [
            dot(self.matrix[0], point),
            dot(self.matrix[1], point),
            dot(self.matrix[2], point),
        ]
    }

    fn apply(self, point: [f32; 3]) -> [f32; 3] {
        let point = self.apply_direction(point);
        [
            point[0] + self.translation[0],
            point[1] + self.translation[1],
            point[2] + self.translation[2],
        ]
    }

    fn apply_normal(self, normal: [f32; 3]) -> [f32; 3] {
        let transformed = [
            self.matrix[0][0] * normal[0]
                + self.matrix[0][1] * normal[1]
                + self.matrix[0][2] * normal[2],
            self.matrix[1][0] * normal[0]
                + self.matrix[1][1] * normal[1]
                + self.matrix[1][2] * normal[2],
            self.matrix[2][0] * normal[0]
                + self.matrix[2][1] * normal[1]
                + self.matrix[2][2] * normal[2],
        ];
        let length = (transformed[0] * transformed[0]
            + transformed[1] * transformed[1]
            + transformed[2] * transformed[2])
            .sqrt();
        if length > 1.0e-8 {
            transformed.map(|value| value / length)
        } else {
            normal
        }
    }
}

pub fn parse_scene(bytes: &[u8]) -> Result<Scene, String> {
    let document = if is_compiled(bytes) {
        parse_binary(bytes)?
    } else {
        parse_ascii(bytes)?
    };
    document.scene()
}

pub fn decompile(bytes: &[u8]) -> Result<String, String> {
    if !is_compiled(bytes) {
        if !looks_ascii(bytes) {
            return Err("not a recognizable Aurora MDL".into());
        }
        return Ok(String::from_utf8_lossy(bytes).replace("\r\n", "\n"));
    }
    Ok(parse_binary(bytes)?.to_ascii())
}

fn is_compiled(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && bytes[..4] == [0, 0, 0, 0]
}

fn looks_ascii(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(16 * 1024)];
    let text = String::from_utf8_lossy(sample).to_ascii_lowercase();
    text.contains("newmodel ") || text.contains("beginmodelgeom ")
}

impl Document {
    fn scene(&self) -> Result<Scene, String> {
        // Resolve the node hierarchy in linear time. The previous repeated scan of all
        // unresolved nodes became quadratic for a deeply nested or malformed model.
        let mut names = BTreeMap::<String, usize>::new();
        for (index, node) in self.nodes.iter().enumerate() {
            names.insert(node.name.to_ascii_lowercase(), index);
        }
        let mut children = vec![Vec::<usize>::new(); self.nodes.len()];
        let mut roots = VecDeque::new();
        for (index, node) in self.nodes.iter().enumerate() {
            let parent = node
                .parent
                .as_ref()
                .filter(|parent| !parent.eq_ignore_ascii_case("null"))
                .and_then(|parent| names.get(&parent.to_ascii_lowercase()).copied());
            if let Some(parent) = parent.filter(|parent| *parent != index) {
                children[parent].push(index);
            } else {
                roots.push_back(index);
            }
        }
        let mut transforms = vec![None; self.nodes.len()];
        while let Some(index) = roots.pop_front() {
            if transforms[index].is_some() {
                continue;
            }
            let node = &self.nodes[index];
            let parent_transform = node
                .parent
                .as_ref()
                .and_then(|parent| names.get(&parent.to_ascii_lowercase()))
                .and_then(|parent| transforms[*parent])
                .unwrap_or(Transform::IDENTITY);
            transforms[index] = Some(parent_transform.compose(Transform::from_node(node)));
            roots.extend(children[index].iter().copied());
        }
        // Cycles are invalid, but rendering their nodes with local transforms is safer than
        // hanging or recursively overflowing the stack.
        for (index, node) in self.nodes.iter().enumerate() {
            transforms[index].get_or_insert_with(|| Transform::from_node(node));
        }

        let mut meshes = Vec::new();
        let mut vertex_count = 0usize;
        let mut face_count = 0usize;
        let mut bounds_min = [f32::INFINITY; 3];
        let mut bounds_max = [f32::NEG_INFINITY; 3];
        for (index, node) in self.nodes.iter().enumerate() {
            if node.vertices.is_empty() || node.faces.is_empty() || node.render == 0 {
                continue;
            }
            let transform = transforms[index].unwrap_or_else(|| Transform::from_node(node));
            let vertices: Vec<[f32; 3]> = node
                .vertices
                .iter()
                .copied()
                .map(|vertex| transform.apply(vertex))
                .collect();
            let normals = node
                .normals
                .iter()
                .copied()
                .map(|normal| transform.apply_normal(normal))
                .collect();
            for vertex in &vertices {
                for axis in 0..3 {
                    bounds_min[axis] = bounds_min[axis].min(vertex[axis]);
                    bounds_max[axis] = bounds_max[axis].max(vertex[axis]);
                }
            }
            let faces: Vec<[u32; 3]> = node.faces.iter().map(|face| face.vertices).collect();
            let texture_faces = node
                .faces
                .iter()
                .map(|face| face.texture_vertices)
                .collect();
            vertex_count = vertex_count.saturating_add(vertices.len());
            face_count = face_count.saturating_add(faces.len());
            meshes.push(SceneMesh {
                vertices,
                normals,
                faces,
                texture_vertices: node.tverts.clone(),
                texture_faces,
                // Enhanced Edition materials normally use the material resref
                // as their diffuse texture name (texture0). Keep direct bitmap
                // declarations authoritative, then fall back to the material.
                texture_name: node.bitmap.clone().or_else(|| node.material_name.clone()),
                color: node.diffuse.map(|value| value.clamp(0.08, 1.0)),
            });
        }
        if meshes.is_empty() {
            return Err("This model does not contain renderable static mesh geometry".into());
        }
        Ok(Scene {
            name: self.name.clone(),
            meshes,
            vertex_count,
            face_count,
            bounds_min,
            bounds_max,
        })
    }

    fn to_ascii(&self) -> String {
        use std::fmt::Write as _;
        let mut output = String::new();
        let _ = writeln!(output, "#MAXMODEL ASCII");
        let _ = writeln!(output, "# Decompiled by Aurora Hak Explorer");
        let _ = writeln!(output, "filedependancy {}.max", self.name);
        let _ = writeln!(output, "newmodel {}", self.name);
        let _ = writeln!(
            output,
            "setsupermodel {} {}",
            self.name,
            if self.supermodel.is_empty() {
                "NULL"
            } else {
                &self.supermodel
            }
        );
        if let Some(classification) = self.classification {
            let _ = writeln!(output, "classification {classification}");
        }
        let _ = writeln!(output, "setanimationscale {:.7}", self.animation_scale);
        if self.ignore_fog {
            let _ = writeln!(output, "ignorefog 1");
        }
        let _ = writeln!(output, "#MAXGEOM ASCII");
        let _ = writeln!(output, "beginmodelgeom {}", self.name);
        for node in &self.nodes {
            node.write_ascii(&mut output);
        }
        let _ = writeln!(output, "endmodelgeom {}", self.name);
        let _ = writeln!(output);
        let _ = writeln!(output, "donemodel {}", self.name);
        output
    }
}

impl Node {
    fn write_ascii(&self, output: &mut String) {
        use std::fmt::Write as _;
        let kind = if self.kind == "skin" {
            "trimesh"
        } else {
            self.kind
        };
        let _ = writeln!(output, "node {kind} {}", self.name);
        let _ = writeln!(
            output,
            "  parent {}",
            self.parent.as_deref().unwrap_or("NULL")
        );
        if self.position != [0.0; 3] {
            let _ = writeln!(
                output,
                "  position {:.7} {:.7} {:.7}",
                self.position[0], self.position[1], self.position[2]
            );
        }
        if self.orientation != [0.0, 0.0, 0.0, 1.0] {
            let orientation = quaternion_to_axis_angle(self.orientation);
            let _ = writeln!(
                output,
                "  orientation {:.7} {:.7} {:.7} {:.7}",
                orientation[0], orientation[1], orientation[2], orientation[3]
            );
        }
        if (self.scale - 1.0).abs() > 1.0e-6 {
            let _ = writeln!(output, "  scale {:.7}", self.scale);
        }
        if !self.vertices.is_empty() {
            let _ = writeln!(
                output,
                "  ambient {:.7} {:.7} {:.7}",
                self.ambient[0], self.ambient[1], self.ambient[2]
            );
            let _ = writeln!(
                output,
                "  diffuse {:.7} {:.7} {:.7}",
                self.diffuse[0], self.diffuse[1], self.diffuse[2]
            );
            let _ = writeln!(
                output,
                "  specular {:.7} {:.7} {:.7}",
                self.specular[0], self.specular[1], self.specular[2]
            );
            let _ = writeln!(output, "  shininess {:.7}", self.shininess);
            let _ = writeln!(output, "  shadow {}", self.shadow);
            let _ = writeln!(output, "  render {}", self.render);
            if let Some(bitmap) = &self.bitmap {
                let _ = writeln!(output, "  bitmap {bitmap}");
            }
            if let Some(material_name) = &self.material_name {
                let _ = writeln!(output, "  materialname {material_name}");
            }
            let _ = writeln!(output, "  verts {}", self.vertices.len());
            for vertex in &self.vertices {
                let _ = writeln!(
                    output,
                    "    {:.7} {:.7} {:.7}",
                    vertex[0], vertex[1], vertex[2]
                );
            }
            if !self.tverts.is_empty() {
                let _ = writeln!(output, "  tverts {}", self.tverts.len());
                for vertex in &self.tverts {
                    let _ = writeln!(output, "    {:.7} {:.7} 0", vertex[0], vertex[1]);
                }
            }
            let _ = writeln!(output, "  faces {}", self.faces.len());
            for face in &self.faces {
                let texture = if self.tverts.is_empty() {
                    [0; 3]
                } else {
                    face.texture_vertices
                };
                let _ = writeln!(
                    output,
                    "    {} {} {}  1  {} {} {}  {}",
                    face.vertices[0],
                    face.vertices[1],
                    face.vertices[2],
                    texture[0],
                    texture[1],
                    texture[2],
                    face.surface
                );
            }
        }
        let _ = writeln!(output, "endnode");
    }
}

fn parse_binary(bytes: &[u8]) -> Result<Document, String> {
    if bytes.len() < HEADER_SIZE + MODEL_HEADER_SIZE || !is_compiled(bytes) {
        return Err("The compiled model header is incomplete".into());
    }
    let model_size = read_u32(bytes, 4)? as usize;
    let raw_size = read_u32(bytes, 8)? as usize;
    let end = HEADER_SIZE
        .checked_add(model_size)
        .and_then(|value| value.checked_add(raw_size))
        .ok_or_else(|| "The compiled model size fields overflow".to_owned())?;
    if end > bytes.len() {
        return Err("The compiled model data ranges exceed the resource size".into());
    }
    let model = &bytes[HEADER_SIZE..HEADER_SIZE + model_size];
    let raw = &bytes[HEADER_SIZE + model_size..end];
    let name = read_string(model, 0x08, 64)?;
    let root = read_u32(model, 0x48)? as usize;
    let declared_nodes = read_u32(model, 0x4c)? as usize;
    if declared_nodes > MAX_NODES {
        return Err("The model declares too many nodes".into());
    }
    let classification = match read_u8(model, 0x72)? {
        1 => Some("EFFECT"),
        2 => Some("TILE"),
        4 => Some("CHARACTER"),
        8 => Some("DOOR"),
        _ => None,
    };
    let supermodel = read_string(model, 0xa8, 64)?;
    let animation_scale = read_f32(model, 0xa4)?;
    let ignore_fog = read_u8(model, 0x73)? == 0;
    let mut nodes = Vec::new();
    let mut visited = BTreeSet::new();
    let mut budget = ParseBudget::default();
    if root != 0 || declared_nodes > 0 {
        let mut pending = vec![(root, None::<String>)];
        while let Some((offset, parent)) = pending.pop() {
            if !visited.insert(offset) {
                continue;
            }
            if visited.len() > MAX_NODES {
                return Err("The model contains too many nodes".into());
            }
            let (node, children) = parse_binary_node(model, raw, offset, parent, &mut budget)?;
            let parent = node.name.clone();
            nodes
                .try_reserve(1)
                .map_err(|_| "The model node list could not be allocated".to_owned())?;
            nodes.push(node);
            pending
                .try_reserve(children.len())
                .map_err(|_| "The model traversal queue could not be allocated".to_owned())?;
            pending.extend(
                children
                    .into_iter()
                    .rev()
                    .map(|child| (child, Some(parent.clone()))),
            );
        }
    }
    Ok(Document {
        name: if name.is_empty() {
            "unnamed".into()
        } else {
            name
        },
        supermodel,
        classification,
        animation_scale,
        ignore_fog,
        nodes,
    })
}

#[derive(Default)]
struct ParseBudget {
    vertices: usize,
    faces: usize,
    controller_keys: usize,
}

fn claim_budget(
    total: &mut usize,
    amount: usize,
    maximum: usize,
    description: &str,
) -> Result<(), String> {
    *total = total
        .checked_add(amount)
        .filter(|total| *total <= maximum)
        .ok_or_else(|| format!("The model contains too many {description}"))?;
    Ok(())
}

fn parse_binary_node(
    model: &[u8],
    raw: &[u8],
    offset: usize,
    parent: Option<String>,
    budget: &mut ParseBudget,
) -> Result<(Node, Vec<usize>), String> {
    checked(model, offset, NODE_HEADER_SIZE, "node header")?;
    let flags = read_u32(model, offset + 0x6c)?;
    let name = read_string(model, offset + 0x20, 32)?;
    let name = if name.is_empty() {
        format!("node_{offset:x}")
    } else {
        name
    };
    let mut node = Node {
        kind: node_kind(flags),
        name: name.clone(),
        parent,
        position: [0.0; 3],
        orientation: [0.0, 0.0, 0.0, 1.0],
        scale: 1.0,
        ambient: [0.2; 3],
        diffuse: [0.7; 3],
        specular: [0.0; 3],
        shininess: 0.0,
        shadow: 1,
        render: 1,
        bitmap: None,
        material_name: None,
        vertices: Vec::new(),
        normals: Vec::new(),
        tverts: Vec::new(),
        faces: Vec::new(),
    };
    read_controllers(model, offset, &mut node, budget)?;
    if flags & HAS_MESH != 0 {
        checked(model, offset, MESH_HEADER_SIZE, "mesh header")?;
        node.ambient = read_vec3(model, offset + 0xb8)?;
        node.diffuse = read_vec3(model, offset + 0xac)?;
        node.specular = read_vec3(model, offset + 0xc4)?;
        node.shininess = read_f32(model, offset + 0xd0)?;
        node.shadow = read_u32(model, offset + 0xd4)?;
        node.render = read_u32(model, offset + 0xdc)?;
        let bitmap = read_string(model, offset + 0xe8, 64)?;
        node.bitmap =
            (!bitmap.is_empty() && !bitmap.eq_ignore_ascii_case("null")).then_some(bitmap);
        let material_name = read_string(model, offset + 0x1a8, 64)?;
        node.material_name = (!material_name.is_empty()
            && !material_name.eq_ignore_ascii_case("null"))
        .then_some(material_name);
        let vertex_offset = read_u32(model, offset + 0x22c)?;
        let vertex_count = read_u16(model, offset + 0x230)? as usize;
        if vertex_count > MAX_VERTICES {
            return Err("A model mesh contains too many vertices".into());
        }
        if vertex_offset != u32::MAX && vertex_count > 0 {
            claim_budget(&mut budget.vertices, vertex_count, MAX_VERTICES, "vertices")?;
            checked(
                raw,
                vertex_offset as usize,
                vertex_count.saturating_mul(12),
                "mesh vertices",
            )?;
            node.vertices
                .try_reserve(vertex_count)
                .map_err(|_| "The model vertex list could not be allocated".to_owned())?;
            for index in 0..vertex_count {
                node.vertices
                    .push(read_vec3(raw, vertex_offset as usize + index * 12)?);
            }
        }
        let tvert_offset = read_u32(model, offset + 0x234)?;
        if tvert_offset != u32::MAX && vertex_count > 0 {
            checked(
                raw,
                tvert_offset as usize,
                vertex_count.saturating_mul(8),
                "mesh texture vertices",
            )?;
            node.tverts
                .try_reserve(vertex_count)
                .map_err(|_| "The model texture coordinates could not be allocated".to_owned())?;
            for index in 0..vertex_count {
                node.tverts.push([
                    read_f32(raw, tvert_offset as usize + index * 8)?,
                    read_f32(raw, tvert_offset as usize + index * 8 + 4)?,
                ]);
            }
        }
        let normal_offset = read_u32(model, offset + 0x244)?;
        if normal_offset != u32::MAX && vertex_count > 0 {
            checked(
                raw,
                normal_offset as usize,
                vertex_count.saturating_mul(12),
                "mesh normals",
            )?;
            node.normals
                .try_reserve(vertex_count)
                .map_err(|_| "The model normal list could not be allocated".to_owned())?;
            for index in 0..vertex_count {
                node.normals
                    .push(read_vec3(raw, normal_offset as usize + index * 12)?);
            }
        }
        let (face_offset, face_count) = read_array(model, offset + 0x78)?;
        if face_count > MAX_FACES {
            return Err("A model mesh contains too many faces".into());
        }
        claim_budget(&mut budget.faces, face_count, MAX_FACES, "faces")?;
        checked(
            model,
            face_offset,
            face_count.saturating_mul(32),
            "mesh faces",
        )?;
        node.faces
            .try_reserve(face_count)
            .map_err(|_| "The model face list could not be allocated".to_owned())?;
        for index in 0..face_count {
            let face = face_offset + index * 32;
            let vertices = [
                read_u16(model, face + 0x1a)? as u32,
                read_u16(model, face + 0x1c)? as u32,
                read_u16(model, face + 0x1e)? as u32,
            ];
            if vertices
                .iter()
                .all(|value| (*value as usize) < vertex_count)
            {
                node.faces.push(Face {
                    vertices,
                    texture_vertices: vertices,
                    surface: read_u32(model, face + 0x10)?,
                });
            }
        }
        // The face table also contains topology used by the engine's tools.
        // NWN Explorer renders the optimized raw index lists instead, which
        // can omit helper/leftover faces that otherwise appear as long rays.
        if let Ok(render_faces) = read_render_faces(model, raw, offset, vertex_count)
            && !render_faces.is_empty()
        {
            budget.faces = budget.faces.saturating_sub(face_count);
            claim_budget(&mut budget.faces, render_faces.len(), MAX_FACES, "faces")?;
            node.faces = render_faces;
        }
    }
    let (children_offset, children_count) = read_array(model, offset + 0x48)?;
    if children_count > MAX_NODES {
        return Err("A model node has too many children".into());
    }
    checked(
        model,
        children_offset,
        children_count.saturating_mul(4),
        "node children",
    )?;
    let mut children = Vec::new();
    children
        .try_reserve(children_count)
        .map_err(|_| "The model child list could not be allocated".to_owned())?;
    for index in 0..children_count {
        children.push(read_u32(model, children_offset + index * 4)? as usize);
    }
    Ok((node, children))
}

fn read_render_faces(
    model: &[u8],
    raw: &[u8],
    node_offset: usize,
    vertex_count: usize,
) -> Result<Vec<Face>, String> {
    let (counts_offset, group_count) = read_array(model, node_offset + 0x204)?;
    let (pointers_offset, pointer_count) = read_array(model, node_offset + 0x210)?;
    if group_count == 0 || group_count != pointer_count || group_count > MAX_FACES {
        return Ok(Vec::new());
    }
    checked(model, counts_offset, group_count * 4, "render index counts")?;
    checked(
        model,
        pointers_offset,
        pointer_count * 4,
        "render index pointers",
    )?;
    let mut faces = Vec::new();
    for group in 0..group_count {
        let index_count = read_u32(model, counts_offset + group * 4)? as usize;
        if index_count > MAX_FACES.saturating_mul(3) {
            return Err("A model mesh contains too many render indices".into());
        }
        let raw_offset = read_u32(model, pointers_offset + group * 4)? as usize;
        checked(raw, raw_offset, index_count * 2, "render indices")?;
        faces
            .try_reserve(index_count / 3)
            .map_err(|_| "The model render face list could not be allocated".to_owned())?;
        for triangle in (0..index_count.saturating_sub(2)).step_by(3) {
            let vertices = [
                read_u16(raw, raw_offset + triangle * 2)? as u32,
                read_u16(raw, raw_offset + (triangle + 1) * 2)? as u32,
                read_u16(raw, raw_offset + (triangle + 2) * 2)? as u32,
            ];
            if vertices
                .iter()
                .all(|vertex| (*vertex as usize) < vertex_count)
            {
                faces.push(Face {
                    vertices,
                    texture_vertices: vertices,
                    surface: 0,
                });
            }
        }
    }
    Ok(faces)
}

fn read_controllers(
    model: &[u8],
    node_offset: usize,
    node: &mut Node,
    budget: &mut ParseBudget,
) -> Result<(), String> {
    let (keys_offset, keys_count) = read_array(model, node_offset + 0x54)?;
    let (data_offset, data_count) = read_array(model, node_offset + 0x60)?;
    claim_budget(
        &mut budget.controller_keys,
        keys_count,
        MAX_CONTROLLER_KEYS,
        "controller keys",
    )?;
    checked(
        model,
        keys_offset,
        keys_count.saturating_mul(12),
        "controller keys",
    )?;
    checked(
        model,
        data_offset,
        data_count.saturating_mul(4),
        "controller data",
    )?;
    for index in 0..keys_count {
        let key = keys_offset + index * 12;
        let kind = read_i32(model, key)?;
        let value_offset = read_i16(model, key + 8)?;
        if value_offset < 0 {
            continue;
        }
        let value = data_offset + value_offset as usize * 4;
        match kind {
            8 if value + 12 <= model.len() => node.position = read_vec3(model, value)?,
            20 if value + 16 <= model.len() => {
                node.orientation = [
                    read_f32(model, value)?,
                    read_f32(model, value + 4)?,
                    read_f32(model, value + 8)?,
                    read_f32(model, value + 12)?,
                ]
            }
            36 if value + 4 <= model.len() => node.scale = read_f32(model, value)?,
            _ => {}
        }
    }
    Ok(())
}

fn parse_ascii(bytes: &[u8]) -> Result<Document, String> {
    if !looks_ascii(bytes) {
        return Err("The resource is not recognizable ASCII MDL source".into());
    }
    let text = String::from_utf8_lossy(bytes);
    let mut name = "unnamed".to_owned();
    let mut supermodel = String::new();
    let mut classification = None;
    let mut animation_scale = 1.0;
    let mut ignore_fog = false;
    let mut nodes = Vec::new();
    let mut current: Option<Node> = None;
    let mut total_vertices = 0usize;
    let mut total_faces = 0usize;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let values: Vec<&str> = line.split_whitespace().collect();
        if values.is_empty() || values[0].starts_with('#') {
            continue;
        }
        match values[0].to_ascii_lowercase().as_str() {
            "newmodel" if values.len() >= 2 => name = values[1].to_owned(),
            "setsupermodel" if values.len() >= 3 => supermodel = values[2].to_owned(),
            "classification" if values.len() >= 2 => {
                classification = match values[1].to_ascii_uppercase().as_str() {
                    "EFFECT" => Some("EFFECT"),
                    "TILE" => Some("TILE"),
                    "CHARACTER" => Some("CHARACTER"),
                    "DOOR" => Some("DOOR"),
                    _ => None,
                }
            }
            "setanimationscale" if values.len() >= 2 => animation_scale = parse_float(values[1])?,
            "ignorefog" if values.len() >= 2 => ignore_fog = values[1] != "0",
            "node" if values.len() >= 3 => {
                if let Some(node) = current.take() {
                    push_ascii_node(&mut nodes, node)?;
                }
                current = Some(Node {
                    kind: ascii_node_kind(values[1]),
                    name: values[2].to_owned(),
                    parent: None,
                    position: [0.0; 3],
                    orientation: [0.0, 0.0, 0.0, 1.0],
                    scale: 1.0,
                    ambient: [0.2; 3],
                    diffuse: [0.7; 3],
                    specular: [0.0; 3],
                    shininess: 0.0,
                    shadow: 1,
                    render: 1,
                    bitmap: None,
                    material_name: None,
                    vertices: Vec::new(),
                    normals: Vec::new(),
                    tverts: Vec::new(),
                    faces: Vec::new(),
                });
            }
            "endnode" => {
                if let Some(node) = current.take() {
                    push_ascii_node(&mut nodes, node)?;
                }
            }
            command => {
                let Some(node) = current.as_mut() else {
                    continue;
                };
                match command {
                    "parent" if values.len() >= 2 => {
                        node.parent =
                            (!values[1].eq_ignore_ascii_case("null")).then(|| values[1].to_owned())
                    }
                    "position" if values.len() >= 4 => node.position = parse_vec3(&values[1..4])?,
                    "orientation" if values.len() >= 5 => {
                        node.orientation = axis_angle_to_quaternion([
                            parse_float(values[1])?,
                            parse_float(values[2])?,
                            parse_float(values[3])?,
                            parse_float(values[4])?,
                        ])
                    }
                    "scale" if values.len() >= 2 => node.scale = parse_float(values[1])?,
                    "ambient" if values.len() >= 4 => node.ambient = parse_vec3(&values[1..4])?,
                    "diffuse" if values.len() >= 4 => node.diffuse = parse_vec3(&values[1..4])?,
                    "specular" if values.len() >= 4 => node.specular = parse_vec3(&values[1..4])?,
                    "shininess" if values.len() >= 2 => node.shininess = parse_float(values[1])?,
                    "shadow" if values.len() >= 2 => node.shadow = values[1].parse().unwrap_or(1),
                    "render" if values.len() >= 2 => node.render = values[1].parse().unwrap_or(1),
                    "bitmap" if values.len() >= 2 => node.bitmap = Some(values[1].to_owned()),
                    "materialname" if values.len() >= 2 => {
                        node.material_name = Some(values[1].to_owned())
                    }
                    "verts" if values.len() >= 2 => {
                        let count = parse_count(values[1], MAX_VERTICES)?;
                        claim_budget(&mut total_vertices, count, MAX_VERTICES, "vertices")?;
                        node.vertices
                            .try_reserve(count)
                            .map_err(|_| "The model vertex list could not be allocated")?;
                        for _ in 0..count {
                            let row = lines.next().ok_or("The ASCII vertex list is incomplete")?;
                            let row: Vec<&str> = row.split_whitespace().collect();
                            node.vertices.push(parse_vec3(&row)?);
                        }
                    }
                    "tverts" if values.len() >= 2 => {
                        let count = parse_count(values[1], MAX_VERTICES)?;
                        node.tverts
                            .try_reserve(count)
                            .map_err(|_| "The model texture coordinates could not be allocated")?;
                        for _ in 0..count {
                            let row = lines
                                .next()
                                .ok_or("The ASCII texture vertex list is incomplete")?;
                            let row: Vec<&str> = row.split_whitespace().collect();
                            if row.len() < 2 {
                                return Err("An ASCII texture vertex is incomplete".into());
                            }
                            node.tverts
                                .push([parse_float(row[0])?, parse_float(row[1])?]);
                        }
                    }
                    "faces" if values.len() >= 2 => {
                        let count = parse_count(values[1], MAX_FACES)?;
                        claim_budget(&mut total_faces, count, MAX_FACES, "faces")?;
                        node.faces
                            .try_reserve(count)
                            .map_err(|_| "The model face list could not be allocated")?;
                        for _ in 0..count {
                            let row = lines.next().ok_or("The ASCII face list is incomplete")?;
                            let row: Vec<&str> = row.split_whitespace().collect();
                            if row.len() < 3 {
                                return Err("An ASCII face is incomplete".into());
                            }
                            node.faces.push(Face {
                                vertices: [
                                    parse_index(row[0])?,
                                    parse_index(row[1])?,
                                    parse_index(row[2])?,
                                ],
                                texture_vertices: if row.len() >= 7 {
                                    [
                                        parse_index(row[4])?,
                                        parse_index(row[5])?,
                                        parse_index(row[6])?,
                                    ]
                                } else {
                                    [0; 3]
                                },
                                surface: row
                                    .get(7)
                                    .and_then(|value| value.parse().ok())
                                    .unwrap_or(0),
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    if let Some(node) = current {
        push_ascii_node(&mut nodes, node)?;
    }
    Ok(Document {
        name,
        supermodel,
        classification,
        animation_scale,
        ignore_fog,
        nodes,
    })
}

fn push_ascii_node(nodes: &mut Vec<Node>, node: Node) -> Result<(), String> {
    if nodes.len() >= MAX_NODES {
        return Err("The model contains too many nodes".into());
    }
    nodes
        .try_reserve(1)
        .map_err(|_| "The model node list could not be allocated".to_owned())?;
    nodes.push(node);
    Ok(())
}

fn node_kind(flags: u32) -> &'static str {
    match flags {
        0x01 => "dummy",
        0x03 => "light",
        0x05 => "emitter",
        0x09 => "camera",
        0x11 => "reference",
        0x21 => "trimesh",
        0x61 => "skin",
        0xa1 => "animmesh",
        0x121 => "danglymesh",
        0x221 => "aabb",
        _ if flags & HAS_MESH != 0 => "trimesh",
        _ => "dummy",
    }
}

fn ascii_node_kind(value: &str) -> &'static str {
    match value.to_ascii_lowercase().as_str() {
        "light" => "light",
        "emitter" => "emitter",
        "camera" => "camera",
        "reference" => "reference",
        "trimesh" => "trimesh",
        "skin" => "skin",
        "animmesh" => "animmesh",
        "danglymesh" => "danglymesh",
        "aabb" => "aabb",
        _ => "dummy",
    }
}

fn normalize_quaternion(value: [f32; 4]) -> [f32; 4] {
    let length = value.iter().map(|part| part * part).sum::<f32>().sqrt();
    if length.is_finite() && length > 1.0e-6 {
        value.map(|part| part / length)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

fn axis_angle_to_quaternion(value: [f32; 4]) -> [f32; 4] {
    let axis_length = (value[0] * value[0] + value[1] * value[1] + value[2] * value[2]).sqrt();
    if !axis_length.is_finite() || axis_length < 1.0e-6 {
        return [0.0, 0.0, 0.0, 1.0];
    }
    let half = value[3] * 0.5;
    let sine = half.sin() / axis_length;
    normalize_quaternion([
        value[0] * sine,
        value[1] * sine,
        value[2] * sine,
        half.cos(),
    ])
}

fn quaternion_to_axis_angle(value: [f32; 4]) -> [f32; 4] {
    let [x, y, z, w] = normalize_quaternion(value);
    let angle = 2.0 * w.clamp(-1.0, 1.0).acos();
    let sine = (1.0 - w * w).max(0.0).sqrt();
    if sine < 1.0e-6 {
        [0.0, 0.0, 1.0, 0.0]
    } else {
        [x / sine, y / sine, z / sine, angle]
    }
}

fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

fn read_array(bytes: &[u8], offset: usize) -> Result<(usize, usize), String> {
    checked(bytes, offset, 12, "array header")?;
    Ok((
        read_u32(bytes, offset)? as usize,
        read_u32(bytes, offset + 4)? as usize,
    ))
}

fn checked(bytes: &[u8], offset: usize, length: usize, context: &str) -> Result<(), String> {
    if offset
        .checked_add(length)
        .is_none_or(|end| end > bytes.len())
    {
        Err(format!("The {context} exceeds the model data"))
    } else {
        Ok(())
    }
}

fn read_u8(bytes: &[u8], offset: usize) -> Result<u8, String> {
    bytes
        .get(offset)
        .copied()
        .ok_or_else(|| "Unexpected end of model data".into())
}
fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    checked(bytes, offset, 2, "integer")?;
    Ok(u16::from_le_bytes(
        bytes[offset..offset + 2].try_into().unwrap(),
    ))
}
fn read_i16(bytes: &[u8], offset: usize) -> Result<i16, String> {
    checked(bytes, offset, 2, "integer")?;
    Ok(i16::from_le_bytes(
        bytes[offset..offset + 2].try_into().unwrap(),
    ))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    checked(bytes, offset, 4, "integer")?;
    Ok(u32::from_le_bytes(
        bytes[offset..offset + 4].try_into().unwrap(),
    ))
}
fn read_i32(bytes: &[u8], offset: usize) -> Result<i32, String> {
    checked(bytes, offset, 4, "integer")?;
    Ok(i32::from_le_bytes(
        bytes[offset..offset + 4].try_into().unwrap(),
    ))
}
fn read_f32(bytes: &[u8], offset: usize) -> Result<f32, String> {
    checked(bytes, offset, 4, "floating-point value")?;
    let value = f32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap());
    if value.is_finite() {
        Ok(value)
    } else {
        Err("The model contains a non-finite number".into())
    }
}
fn read_vec3(bytes: &[u8], offset: usize) -> Result<[f32; 3], String> {
    Ok([
        read_f32(bytes, offset)?,
        read_f32(bytes, offset + 4)?,
        read_f32(bytes, offset + 8)?,
    ])
}
fn read_string(bytes: &[u8], offset: usize, length: usize) -> Result<String, String> {
    checked(bytes, offset, length, "string")?;
    let field = &bytes[offset..offset + length];
    let end = field
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(field.len());
    Ok(String::from_utf8_lossy(&field[..end]).trim().to_owned())
}
fn parse_float(value: &str) -> Result<f32, String> {
    value
        .parse::<f32>()
        .map_err(|_| format!("Invalid MDL number: {value}"))
}
fn parse_vec3(values: &[&str]) -> Result<[f32; 3], String> {
    if values.len() < 3 {
        return Err("An ASCII vector is incomplete".into());
    }
    Ok([
        parse_float(values[0])?,
        parse_float(values[1])?,
        parse_float(values[2])?,
    ])
}
fn parse_count(value: &str, maximum: usize) -> Result<usize, String> {
    let count = value
        .parse::<usize>()
        .map_err(|_| format!("Invalid MDL count: {value}"))?;
    if count > maximum {
        Err("The model contains an impractical element count".into())
    } else {
        Ok(count)
    }
}
fn parse_index(value: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|_| format!("Invalid MDL index: {value}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ascii_triangle_for_preview() {
        let source = b"newmodel test\nsetsupermodel test NULL\nbeginmodelgeom test\nnode trimesh mesh\n parent NULL\n verts 3\n 0 0 0\n 1 0 0\n 0 1 0\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nendmodelgeom test\ndonemodel test\n";
        let scene = parse_scene(source).unwrap();
        assert_eq!(scene.vertex_count, 3);
        assert_eq!(scene.face_count, 1);
    }

    #[test]
    fn uses_enhanced_edition_material_as_texture_fallback() {
        let source = b"newmodel material_test\nbeginmodelgeom material_test\nnode trimesh mesh\n parent NULL\n materialname cm_nazgsword\n verts 3\n 0 0 0\n 1 0 0\n 0 1 0\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nendmodelgeom material_test\ndonemodel material_test\n";
        let scene = parse_scene(source).unwrap();
        assert_eq!(
            scene.meshes[0].texture_name.as_deref(),
            Some("cm_nazgsword")
        );
    }

    #[test]
    fn rejects_truncated_binary_models() {
        assert!(parse_scene(&[0; 12]).is_err());
        assert!(decompile(&[0; 12]).is_err());
    }

    #[test]
    fn round_trips_axis_angle_orientation() {
        let source = [0.0, 0.0, 1.0, std::f32::consts::FRAC_PI_2];
        let result = quaternion_to_axis_angle(axis_angle_to_quaternion(source));
        for (actual, expected) in result.into_iter().zip(source) {
            assert!((actual - expected).abs() < 1.0e-5);
        }
    }

    #[test]
    fn malformed_binary_inputs_never_panic() {
        for length in 0..512 {
            let bytes: Vec<u8> = (0..length)
                .map(|index| (index as u8).wrapping_mul(37).wrapping_add(length as u8))
                .collect();
            let result = std::panic::catch_unwind(|| parse_scene(&bytes));
            assert!(
                result.is_ok(),
                "parser panicked for an input of {length} bytes"
            );
        }
    }

    #[test]
    fn cyclic_ascii_hierarchy_does_not_hang() {
        let source = b"newmodel cycle\nbeginmodelgeom cycle\nnode trimesh first\n parent second\n verts 3\n 0 0 0\n 1 0 0\n 0 1 0\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nnode trimesh second\n parent first\n verts 3\n 0 0 1\n 1 0 1\n 0 1 1\n faces 1\n 0 1 2 1 0 1 2 0\nendnode\nendmodelgeom cycle\ndonemodel cycle\n";
        let scene = parse_scene(source).unwrap();
        assert_eq!(scene.vertex_count, 6);
        assert_eq!(scene.face_count, 2);
    }
}
