use super::*;

// Clustered-forward rendering notes
// The main initial reference material used was this rather accessible article:
// http://www.aortiz.me/2018/12/21/CG.html
// Some inspiration was taken from “Practical Clustered Shading” which is part 2 of:
// https://efficientshading.com/2015/01/01/real-time-many-light-management-and-shadows-with-clustered-shading/
// (Also note that Part 3 of the above shows how we could support the shadow mapping for many lights.)
// The z-slicing method mentioned in the aortiz article is originally from Tiago Sousa's Siggraph 2016 talk about Doom 2016:
// http://advances.realtimerendering.com/s2016/Siggraph2016_idTech6.pdf

/// Configure the far z-plane mode used for the furthest depth slice for clustered forward
/// rendering
#[derive(Debug, Copy, Clone, Reflect)]
pub enum ClusterFarZMode {
    /// Calculate the required maximum z-depth based on currently visible lights.
    /// Makes better use of available clusters, speeding up GPU lighting operations
    /// at the expense of some CPU time and using more indices in the cluster light
    /// index lists.
    MaxLightRange,
    /// Constant max z-depth
    Constant(f32),
}

/// Configure the depth-slicing strategy for clustered forward rendering
#[derive(Debug, Copy, Clone, Reflect)]
#[reflect(Default)]
pub struct ClusterZConfig {
    /// Far `Z` plane of the first depth slice
    pub first_slice_depth: f32,
    /// Strategy for how to evaluate the far `Z` plane of the furthest depth slice
    pub far_z_mode: ClusterFarZMode,
}

impl Default for ClusterZConfig {
    fn default() -> Self {
        Self {
            first_slice_depth: 5.0,
            far_z_mode: ClusterFarZMode::MaxLightRange,
        }
    }
}

/// Configuration of the clustering strategy for clustered forward rendering
#[derive(Debug, Copy, Clone, Component, Reflect)]
#[reflect(Component)]
pub enum ClusterConfig {
    /// Disable light cluster calculations for this view
    None,
    /// One single cluster. Optimal for low-light complexity scenes or scenes where
    /// most lights affect the entire scene.
    Single,
    /// Explicit `X`, `Y` and `Z` counts (may yield non-square `X/Y` clusters depending on the aspect ratio)
    XYZ {
        dimensions: UVec3,
        z_config: ClusterZConfig,
        /// Specify if clusters should automatically resize in `X/Y` if there is a risk of exceeding
        /// the available cluster-light index limit
        dynamic_resizing: bool,
    },
    /// Fixed number of `Z` slices, `X` and `Y` calculated to give square clusters
    /// with at most total clusters. For top-down games where lights will generally always be within a
    /// short depth range, it may be useful to use this configuration with 1 or few `Z` slices. This
    /// would reduce the number of lights per cluster by distributing more clusters in screen space
    /// `X/Y` which matches how lights are distributed in the scene.
    FixedZ {
        total: u32,
        z_slices: u32,
        z_config: ClusterZConfig,
        /// Specify if clusters should automatically resize in `X/Y` if there is a risk of exceeding
        /// the available cluster-light index limit
        dynamic_resizing: bool,
    },
}

impl Default for ClusterConfig {
    fn default() -> Self {
        // 24 depth slices, square clusters with at most 4096 total clusters
        // use max light distance as clusters max `Z`-depth, first slice extends to 5.0
        Self::FixedZ {
            total: 4096,
            z_slices: 24,
            z_config: ClusterZConfig::default(),
            dynamic_resizing: true,
        }
    }
}

impl ClusterConfig {
    pub(super) fn dimensions_for_screen_size(&self, screen_size: UVec2) -> UVec3 {
        match &self {
            ClusterConfig::None => UVec3::ZERO,
            ClusterConfig::Single => UVec3::ONE,
            ClusterConfig::XYZ { dimensions, .. } => *dimensions,
            ClusterConfig::FixedZ {
                total, z_slices, ..
            } => {
                let aspect_ratio: f32 =
                    AspectRatio::from_pixels(screen_size.x, screen_size.y).into();
                let mut z_slices = *z_slices;
                if *total < z_slices {
                    warn!("ClusterConfig has more z-slices than total clusters!");
                    z_slices = *total;
                }
                let per_layer = *total as f32 / z_slices as f32;

                let y = f32::sqrt(per_layer / aspect_ratio);

                let mut x = (y * aspect_ratio) as u32;
                let mut y = y as u32;

                // check extremes
                if x == 0 {
                    x = 1;
                    y = per_layer as u32;
                }
                if y == 0 {
                    x = per_layer as u32;
                    y = 1;
                }

                UVec3::new(x, y, z_slices)
            }
        }
    }

    pub(super) fn first_slice_depth(&self) -> f32 {
        match self {
            ClusterConfig::None | ClusterConfig::Single => 0.0,
            ClusterConfig::XYZ { z_config, .. } | ClusterConfig::FixedZ { z_config, .. } => {
                z_config.first_slice_depth
            }
        }
    }

    pub(super) fn far_z_mode(&self) -> ClusterFarZMode {
        match self {
            ClusterConfig::None => ClusterFarZMode::Constant(0.0),
            ClusterConfig::Single => ClusterFarZMode::MaxLightRange,
            ClusterConfig::XYZ { z_config, .. } | ClusterConfig::FixedZ { z_config, .. } => {
                z_config.far_z_mode
            }
        }
    }

    pub(super) fn dynamic_resizing(&self) -> bool {
        match self {
            ClusterConfig::None | ClusterConfig::Single => false,
            ClusterConfig::XYZ {
                dynamic_resizing, ..
            }
            | ClusterConfig::FixedZ {
                dynamic_resizing, ..
            } => *dynamic_resizing,
        }
    }
}

#[derive(Component, Debug, Default)]
pub struct Clusters {
    /// Tile size
    pub(crate) tile_size: UVec2,
    /// Number of clusters in `X` / `Y` / `Z` in the view frustum
    pub(crate) dimensions: UVec3,
    /// Distance to the far plane of the first depth slice. The first depth slice is special
    /// and explicitly-configured to avoid having unnecessarily many slices close to the camera.
    pub(crate) near: f32,
    pub(crate) far: f32,
    pub(crate) lights: Vec<VisiblePointLights>,
}

impl Clusters {
    pub(super) fn update(&mut self, screen_size: UVec2, requested_dimensions: UVec3) {
        debug_assert!(
            requested_dimensions.x > 0 && requested_dimensions.y > 0 && requested_dimensions.z > 0
        );

        let tile_size = (screen_size.as_vec2() / requested_dimensions.xy().as_vec2())
            .ceil()
            .as_uvec2()
            .max(UVec2::ONE);
        self.tile_size = tile_size;
        self.dimensions = (screen_size.as_vec2() / tile_size.as_vec2())
            .ceil()
            .as_uvec2()
            .extend(requested_dimensions.z)
            .max(UVec3::ONE);

        // NOTE: Maximum 4096 clusters due to uniform buffer size constraints
        debug_assert!(self.dimensions.x * self.dimensions.y * self.dimensions.z <= 4096);
    }
    pub(super) fn clear(&mut self) {
        self.tile_size = UVec2::ONE;
        self.dimensions = UVec3::ZERO;
        self.near = 0.0;
        self.far = 0.0;
        self.lights.clear();
    }
}
