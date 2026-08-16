//! Framework adapters for transforming JSX to Web Components.
//!
//! This crate provides the core transformation logic that converts React/Solid JSX
//! components into static Web Components for documentation previews.

pub mod artifact;
pub mod config_interface;
pub mod conventions;
pub mod coverage;
pub mod element;
pub mod generator;
pub mod intelligence;
pub mod matrix;
pub mod mdx;
pub mod mode;
pub mod rafters_source;
pub mod react;
pub mod registry;
pub mod scope;
pub mod substrate;
pub mod traits;
pub(crate) mod ts_helpers;
pub mod veneer_config;

pub use artifact::{
    build_artifact, write_artifact, ArtifactError, FieldValue, IntelligenceArtifact,
    OverrideReason, ARTIFACT_SCHEMA_VERSION,
};
pub use config_interface::{
    attribute_name, parse_config_interface, resolve_config_interface, ConfigInterface,
    ResolvedConfig,
};
pub use conventions::ComponentConventions;
pub use coverage::{assess_coverage, AssessedItem, CoverageReport, CoverageState};
pub use element::{resolve_root_element, ElementError, ElementSource, ResolvedElement};
pub use generator::{
    generate_passthrough_web_component, generate_web_component, preview_styles_module,
    preview_web_component_block, web_component_block, PREVIEW_STYLES_MODULE,
};
pub use intelligence::{
    render_component, CognitiveLoad, CompiledIntelligence, Constraint, ConstraintKind,
    DependencyOrigin, DependencyRef, PropDoc, RenderedComponent, TokenRef, VariantDoc,
};
pub use matrix::{
    default_matrix_path, parse_matrix, read_matrix, Archetype, BehaviorLayer, ComponentLine,
    ComponentMetadata, FileStatus, Frameworks, MatrixCognitiveLoad, MatrixError, Motion,
    PortStatus, Provenance, Uses, COMPONENT_LINE_SCHEMA,
};
pub use mdx::{component_page_file_name, generate_component_page, GeneratedComponentPage};
pub use mode::{detect_mode, dispatch_framework, FrameworkDispatch, Mode};
pub use rafters_source::{
    read_framework_declaration, read_rafters_namespace, read_rafters_stylesheet,
    AccessibilityMatrices, ContrastMatrix, ContrastPair, FrameworkDeclaration, IntelligenceSource,
    NamespaceError, NamespaceFile, NamespaceToken, OklchComponents, RaftersNamespace,
    StructuredValue, TokenValue, UsagePatterns, UserOverride, DOCUMENTATION_SHEET_PATH,
};
pub use react::{ComponentStructure, ReactAdapter};
pub use registry::{
    is_excluded_dir_name, CachedComponent, ComponentRegistry, DiscoveredItem, DiscoveredKind,
    RegistryError,
};
pub use scope::extract_classes_from_ts;
pub use substrate::{
    build_substrate, to_jsonl, DocLine, IndexLine, Substrate, DOC_SCHEMA, INDEX_SCHEMA,
    STOPLIGHT_RULE_VERSION,
};
pub use traits::{FrameworkAdapter, TransformContext, TransformError, TransformedBlock};
pub use veneer_config::{read_veneer_config, Reporters, VeneerConfig, VENEER_CONFIG_VERSION};
