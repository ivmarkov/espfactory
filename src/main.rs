use espfactory::loader::dir::DirLoader;
use espfactory::Config;

extern crate alloc;

fn main() -> anyhow::Result<()> {
    let project_dirs = directories::ProjectDirs::from("org", "ivmarkov", "espfactory")
        .ok_or_else(|| anyhow::anyhow!("Cannot mount project directories"))?;

    let conf_path = project_dirs.config_dir().join("config.toml");
    let conf = if conf_path.exists() {
        let mut conf_str = String::new();
        std::fs::read_to_string(&mut conf_str)?;

        toml::from_str(&conf_str)?
    } else {
        Config {
            port: None,
            bundle_identification: espfactory::BundleIdentification::BoxId,
            test_jig_id_readout: true,
            pcb_id_readout: true,
            box_id_readout: true,
        }
    };

    let bundle_dir = &project_dirs.cache_dir().join("bundle");

    let loader = DirLoader::new(project_dirs.cache_dir().join("bundles") /*TODO*/);

    futures_lite::future::block_on(espfactory::run(&conf, bundle_dir, loader))
}
