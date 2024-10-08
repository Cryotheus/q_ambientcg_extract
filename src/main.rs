mod utils;

use utils::*;

use anyhow::bail;
use image::ImageFormat::Png;
use image::{ColorType, DynamicImage, ImageBuffer, ImageFormat, Rgb};
use indicatif::ParallelProgressIterator;
use rayon::prelude::*;
use smallvec::SmallVec;
use std::env::current_dir;
use std::collections::HashMap;
use std::fs::{self, read_dir, File};
use std::io::{stdin, stdout, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

/// Single lets the image process alone.
/// Dependent means the image requires access to a sibling image.
enum ProcessingMethod {
	Single(ImageBake),
	Dependent,
}

#[derive(Default)]
struct ImageBake {
	rename: &'static str,
	config_lines: Option<SmallVec<[&'static str; 4]>>,
	color: Option<ColorType>,
	edited_image: Option<DynamicImage>,
}

impl ImageBake {
	pub fn config(rename: &'static str, config_multiline: &'static str) -> Self {
		Self {
			rename,
			config_lines: Some(config_multiline.lines().collect()),
			color: None,
			edited_image: None,
		}
	}

	pub fn new(rename: &'static str) -> Self {
		Self {
			rename,
			config_lines: None,
			color: None,
			edited_image: None,
		}
	}

	pub fn from_postfix_path(postfix: &str) -> anyhow::Result<Option<ProcessingMethod>> {
		Ok(Some(match postfix {
			"AmbientOcclusion" => ProcessingMethod::Single(ImageBake::config("ao", "ao = true")),
			"Color" => ProcessingMethod::Single(ImageBake::new("color")),
			"Displacement" => ProcessingMethod::Single(ImageBake::config("depth", "depth = 0.01\ndepth_method = 8")),

			"NormalGL" => ProcessingMethod::Single(ImageBake {
				rename: "normal",
				config_lines: Some(SmallVec::from_vec(["normal = \"OpenGL\""].to_vec())),
				color: Some(ColorType::Rgb16),
				edited_image: None,
			}),

			"Metalness" => ProcessingMethod::Dependent,
			"Roughness" => ProcessingMethod::Dependent,

			_ => return Ok(None),
		}))
	}
}

/// Returns `Ok(())` if correct.
fn correct_extension(path: impl AsRef<Path>) -> Result<(), AcgeError> {
	match path.as_ref().extension().indoc_str()? {
		//TODO: more exts
		//e.g.  | "tga" | "exr"
		//we will need to carry extension data around though...
		"png" => Ok(()),
		extension => Err(AcgeError::InvalidImageFileExtension(extension.into())),
	}
}

fn main() -> anyhow::Result<()> {
	let cwd = current_dir()?;
	let mut zip_paths: Vec<PathBuf> = Vec::new();

	for entry in cwd.read_dir()? {
		let Ok(entry) = entry else {
			continue;
		};

		let Ok(metadata) = entry.metadata() else {
			continue;
		};

		if metadata.is_dir() {
			continue;
		}

		let path = entry.path();

		//check if the extension is zip
		match path.extension() {
			None => continue,
			Some(os_str) => match os_str.to_str() {
				None => continue,
				Some("zip") => {}
				Some(_) => continue,
			},
		}

		zip_paths.push(path);
	}

	zip_paths.sort_unstable();

	//let the user know what's about to get affected
	{
		let mut stdout_lock = stdout().lock();
		stdout_lock.write_all(b"The following zip archives will extracted:\n")?;

		for (index, zip_path) in zip_paths.iter().enumerate() {
			writeln!(
				stdout_lock,
				"{index} \t- {}",
				zip_path.file_name().and_then(|os_str| os_str.to_str()).unwrap_or("<unknown>")
			)?;
		}

		stdout_lock.write_all(b"Continue? (Y/N):\n")?;
	}

	//get confirmation from stdin
	let mut buffer = String::new();

	stdin().lock().read_line(&mut buffer).unwrap();

	if buffer.chars().next().unwrap().to_lowercase().next().unwrap() != 'y' {
		return Ok(());
	}

	//extract and collect the extraction results into a vec
	let results = zip_paths.into_par_iter().progress().map(process_zip).collect::<Vec<_>>();

	//spit out the results
	{
		let mut stdout_lock = stdout().lock();

		for (index, result) in results.iter().enumerate() {
			write!(stdout_lock, "{index}\t")?;

			match result {
				Ok(()) => stdout_lock.write_all(b"[  OK  ]\n")?,
				Err(error) => writeln!(stdout_lock, "[FAILED]\n\t Error: {error:#?}")?,
			}
		}
	}

	Ok(())
}

fn process_zip(zip_path: PathBuf) -> anyhow::Result<()> {
	let extract_dir = zip_path.with_extension("");
	let mut zip_reader = zip::read::ZipArchive::new(BufReader::new(File::open(&zip_path)?))?;

	if extract_dir.exists() {
		if extract_dir.is_file() {
			bail!("extract directory [{extract_dir:?}] already exists as a file");
		} else if extract_dir.read_dir()?.next().is_some() {
			bail!("extract directory [{extract_dir:?}] already has files");
		}
	} else {
		fs::create_dir(&extract_dir)?;
	}

	//could selectively extract...
	//but looking at the inner workings of the extract method makes me think otherwise
	zip_reader.extract(&extract_dir)?;
	drop(zip_reader);

	let mut to_delete = SmallVec::<[PathBuf; 8]>::new();
	let mut file_paths = SmallVec::<[PathBuf; 8]>::new();
	let mut shortest_file_name_index = 0usize;
	let mut shortest_file_name_len = usize::MAX; //will certainly be lowered with any amount of iteration

	//just collect it this time around
	//we will do more "erring" stuff later
	for entry in extract_dir.read_dir()? {
		let Ok(entry) = entry else {
			continue;
		};

		let Ok(metadata) = entry.metadata() else {
			continue;
		};

		let file_path = entry.path();

		if metadata.is_dir() {
			bail!("unexpected sub-directory [{file_path:?}] in extract directory");
		}

		let Ok(file_name) = file_path.file_name().indoc_str() else {
			bail!("failed to convert file name of path [{file_path:?}] in extract directory");
		};

		if correct_extension(&file_path).is_ok() {
			if file_name.len() < shortest_file_name_len {
				shortest_file_name_index = file_paths.len();
				shortest_file_name_len = file_name.len();
			}

			file_paths.push(file_path);
		} else {
			to_delete.push(file_path);
		}
	}

	if file_paths.is_empty() {
		return Err(AcgeError::NoFilesToFilter.into());
	}

	//the shortest file with the shortest name is the thumbnail
	//we don't need it
	to_delete.push(file_paths.remove(shortest_file_name_index));

	//the previous check prevents a crash
	//this check saves time
	if file_paths.is_empty() {
		return Err(AcgeError::NoFilesToFilter.into());
	}

	let mut shortest_common_prefix = file_paths.get(0).ok_or(AcgeError::NoFilesToFilter)?.file_name().indoc_str()?;

	for file_path in &file_paths {
		let file_name = file_path.file_name().indoc_str()?;

		shortest_common_prefix = shortest_common_prefix.common_prefix(file_name);
	}

	//own it so we don't hold a reference to file_paths for too long
	let shortest_common_prefix = shortest_common_prefix.to_string();

	//check if we have a roughness map
	let mut roughness_file_name = shortest_common_prefix.clone();
	roughness_file_name.push_str("Roughness");

	//convert roughness -> specular (if it exists)
	if let Some((index, roughness_file_path)) = file_paths
		.iter()
		.enumerate()
		.find(|(_, file_path)| file_path.file_name().indoc_str().unwrap() == &roughness_file_name)
	{
		let file_reader = BufReader::new(File::open(roughness_file_path)?);
		let mut dyn_image = image::load(file_reader, ImageFormat::Png)?;

		dyn_image.invert();
		dyn_image.save(roughness_file_path.with_file_name("specular.png"))?;
		to_delete.push(file_paths.remove(index));
	}

	let mut config_lines = SmallVec::<[&'static str; 8]>::new();
	let mut multi_process = HashMap::<String, PathBuf>::new();

	config_lines.push("tile = true");

	//rename remaining files
	for file_path in file_paths {
		let postfix = file_path.file_stem().indoc_str()?.split_at(shortest_common_prefix.len()).1;

		if let Some(processing_method) = ImageBake::from_postfix_path(postfix)? {
			match processing_method {
				ProcessingMethod::Single(image_bake) => {
					let new_path = file_path.with_file_name(format!("{}.png", image_bake.rename));

					if let Some(mut append_lines) = image_bake.config_lines {
						config_lines.append(&mut append_lines);
					}

					let fn_color_space_correction = |image: &DynamicImage| -> Option<DynamicImage> {
						if let Some(enforced) = image_bake.color {
							//change the color to whatever is enforced by the ImageBake
							if image.color() == enforced {
								None
							} else {
								let image = image.clone();

								Some(match enforced {
									//isn't there a function for this or something?
									ColorType::L8 => image.into_luma8().into(),
									ColorType::La8 => image.into_luma_alpha8().into(),
									ColorType::Rgb8 => image.into_rgb8().into(),
									ColorType::Rgba8 => image.into_rgba8().into(),
									ColorType::L16 => image.into_luma16().into(),
									ColorType::La16 => image.into_luma_alpha16().into(),
									ColorType::Rgb16 => image.into_rgb16().into(),
									ColorType::Rgba16 => image.into_rgba16().into(),
									ColorType::Rgb32F => image.into_rgb32f().into(),
									ColorType::Rgba32F => image.into_rgba32f().into(),
									color_format => {
										println!("unrecognized color format {color_format:?}");

										return None;
									}
								})
							}
						} else {
							//change the color to something more common
							match image.color() {
								//probably bevy compatible
								ColorType::L8 | ColorType::La8 | ColorType::Rgb8 | ColorType::Rgba8 | ColorType::Rgb32F | ColorType::Rgba32F => None,

								//not bevy compatible (at least not globally, fine for normal maps)
								ColorType::L16 | ColorType::La16 | ColorType::Rgb16 | ColorType::Rgba16 => Some(image.clone().into_rgba8().into()),

								color_format => {
									println!("unrecognized color format {color_format:?}");

									None
								}
							}
						}
					};

					if let Some(mut edited_image) = image_bake.edited_image {
						if let Some(corrected_image) = fn_color_space_correction(&edited_image) {
							edited_image = corrected_image;
						}

						edited_image.save(file_path.with_file_name(new_path))?;
						to_delete.push(file_path);
					} else {
						if let Some(corrected_image) = fn_color_space_correction(&image::load(BufReader::new(File::open(&file_path)?), Png)?) {
							corrected_image.save(file_path.with_file_name(new_path))?;
							to_delete.push(file_path);
						} else {
							fs::rename(file_path, new_path)?
						}
					}
				}

				ProcessingMethod::Dependent => {
					to_delete.push(file_path.clone());
					multi_process.insert(postfix.to_string(), file_path);
				}
			}
		} else {
			to_delete.push(file_path);
		}
	}

	//some textures are dependent on other textures
	//as of right now, this is just the metallic and rough maps
	//bevy combines these into a single texture
	if !multi_process.is_empty() {
		//so... mr. bevy says:
		//  The blue channel contains metallic values, and the green channel contains the roughness values.
		//  Other channels are unused.
		//thus we create our own combo zero-roughness-metal texture
		if let Some(image) = match [multi_process.get("Metalness"), multi_process.get("Roughness")] {
			//metal material
			[Some(metalness_path), Some(roughness_path)] => {
				config_lines.push("metal = 1.0");
				config_lines.push("rough = 1.0");

				let metalness_image = image::load(BufReader::new(File::open(metalness_path)?), Png)?.into_luma8();
				let roughness_image = image::load(BufReader::new(File::open(roughness_path)?), Png)?.into_luma8();
				let [width, height] = [metalness_image.width(), metalness_image.height()];

				if metalness_image.width() != roughness_image.width() || metalness_image.height() != roughness_image.height() {
					bail!(
						"bevy metal image requires matching image sizes [{metalness_path:?}] {}x{} [{roughness_path:?}] {}x{}",
						width,
						height,
						roughness_image.width(),
						roughness_image.height()
					);
				}

				let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_par_fn(width, height, |x, y| {
					Rgb([0u8, roughness_image.get_pixel(x, y).0[0], metalness_image.get_pixel(x, y).0[0]])
				});

				Some(DynamicImage::from(image))
			}

			//rough material
			[None, Some(roughness_path)] => {
				config_lines.push("rough = 1.0");

				let mut roughness_image = image::load(BufReader::new(File::open(roughness_path)?), Png)?.into_rgb8();

				//remove red and blue channel - just green is used
				//red is unused
				//blue is for metal
				roughness_image.par_pixels_mut().for_each(|px| {
					px.0[0] = 0;
					px.0[2] = 0;
				});

				Some(DynamicImage::from(roughness_image))
			}

			//impossible material?
			[Some(metalness_path), None] => bail!("Metalness image [{metalness_path:?}] without roughness map."),
			_ => None,
		} {
			image.save(extract_dir.join("combo_0rm.png"))?;
		}
	}

	//create the material config
	config_lines.sort_unstable();

	let joined = config_lines.join("\n");
	let mut file_handle = File::create(extract_dir.join("material.toml"))?;

	file_handle.write_all(joined.as_bytes())?;
	file_handle.write_all(b"\n")?;

	//batch me glados
	for file_path in to_delete {
		fs::remove_file(file_path)?;
	}

	//rename the folder resulting folder to something simpler
	let mut finished_folder = extract_dir.file_name().indoc_str().unwrap().split_at(shortest_file_name_len).0;

	if finished_folder.ends_with(".png") {
		finished_folder = finished_folder.split_at(finished_folder.len() - 4).0;
	}

	finished_folder = finished_folder.trim_end_matches(['-', '_']);

	if let Some(underscore_index) = finished_folder.rfind("_") {
		if match finished_folder.split_at(underscore_index + 1).1.as_bytes() {
			[first, b'K'] if first.is_ascii_digit() => true,
			[bytes @ .., b'K'] => std::str::from_utf8(bytes).ok().and_then(|str| str.parse::<u8>().ok()).is_some(),
			_ => false,
		} {
			finished_folder = finished_folder.split_at(underscore_index).0;
		}
	}

	finished_folder = finished_folder.trim_end_matches(['-', '_']);
	let finished_path = extract_dir.with_file_name(finished_folder.to_ascii_lowercase());

	//rename the folder
	//annoying impl for windows because... windows.
	cfg_if::cfg_if! {
		if #[cfg(windows)] {
			fs::create_dir(&finished_path)?;

			//move contents of folder to other folder
			for entry in read_dir(&extract_dir)? {
				let Ok(entry) = entry else {
					continue;
				};

				let Ok(meta) = entry.metadata() else {
					continue;
				};

				if meta.is_dir() {
					continue;
				}

				let path = entry.path();

				let Some(file_name) = path.file_name() else {
					continue;
				};

				fs::rename(&path, finished_path.join(file_name))?;
			}

			//delete the old folder
			fs::remove_dir_all(&extract_dir)?;
		} else {
			//linux works fine
			//not sure about macos
			fs::rename(&extract_dir, finished_path)?;
		}
	}

	Ok(())
}
