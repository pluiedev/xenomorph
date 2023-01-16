use std::{
	collections::{HashMap, HashSet},
	fmt::Write as _,
	fs::File,
	io::Write,
	path::{Component, Path, PathBuf},
};

use base64::Engine;
use fs_extra::dir::CopyOptions;
use nix::unistd::{chown, geteuid, Gid, Group, Uid, User};
use simple_eyre::{
	eyre::{bail, Context},
	Result,
};
use subprocess::{Exec, NullFile, Redirection};

use crate::{
	package::Format,
	util::{ExecExt, Verbosity},
	Args,
};

use super::{
	common::{self, chmod},
	PackageBehavior, PackageInfo,
};

pub struct Rpm {
	info: PackageInfo,
	rpm_file: PathBuf,
	prefixes: Option<PathBuf>,
}
impl Rpm {
	pub fn check_file(file: &Path) -> bool {
		match file.extension() {
			Some(o) => o.eq_ignore_ascii_case("rpm"),
			None => false,
		}
	}
	pub fn new(rpm_file: PathBuf, args: &Args) -> Result<Self> {
		// I'm lazy.
		fn rpm() -> Exec {
			Exec::cmd("rpm").env("LANG", "C")
		}
		let read_field = |name: &str| -> Result<Option<String>> {
			let res = rpm()
				.arg("-qp")
				.arg("--queryformat")
				.arg(name)
				.arg(&rpm_file)
				.log_and_output(None)?
				.stdout_str();

			Ok(if res == "(none)" { None } else { Some(res) })
		};

		let prefixes = read_field("%{PREFIXES}")?.map(PathBuf::from);

		// rpm maintainer scripts are typically shell scripts,
		// but often lack the leading shebang line.
		// This can confuse dpkg, so add the shebang if it looks like
		// there is no shebang magic already in place.
		//
		// Additionally, it's not uncommon for rpm maintainer scripts to
		// contain bashisms, which can be triggered when they are run on
		// systems where /bin/sh is not bash. To work around this,
		// the shebang line of the scripts is changed to use bash.
		//
		// Also if the rpm is relocatable, the script could refer to
		// RPM_INSTALL_PREFIX, which is to set by rpm at runtime.
		// Deal with this by adding code to the script to set RPM_INSTALL_PREFIX.

		let sanitize_script = |s: Option<String>| -> Option<String> {
			let prefix_code = prefixes
				.as_ref()
				.map(|p| {
					format!(
						"\nRPM_INSTALL_PREFIX={}\nexport RPM_INSTALL_PREFIX",
						p.display()
					)
				})
				.unwrap_or_default();

			if let Some(t) = &s {
				if let Some(t) = t.strip_prefix("#!") {
					let t = t.trim_start();
					if t.starts_with('/') {
						let mut t = t.replacen("/bin/sh", "#!/bin/bash", 1);
						if let Some(nl) = t.find('\n') {
							t.insert_str(nl, &prefix_code)
						}
						return Some(t);
					}
				}
			}
			Some(format!(
				"#!/bin/bash\n{prefix_code}{}",
				s.unwrap_or_default()
			))
		};

		let mut conffiles: Vec<_> = rpm()
			.arg("-qcp")
			.arg(&rpm_file)
			.log_and_output(None)?
			.stdout_str()
			.lines()
			.map(|s| PathBuf::from(s.trim()))
			.collect();
		if let Some(f) = conffiles.first() {
			if f.as_os_str() == "(contains no files)" {
				conffiles.clear();
			}
		}

		let mut file_list: Vec<_> = rpm()
			.arg("-qlp")
			.arg(&rpm_file)
			.log_and_output(None)?
			.stdout_str()
			.lines()
			.map(|s| PathBuf::from(s.trim()))
			.collect();
		if let Some(f) = file_list.first() {
			if f.as_os_str() == "(contains no files)" {
				file_list.clear();
			}
		}

		let binary_info = rpm()
			.arg("-qip")
			.arg(&rpm_file)
			.log_and_output(None)?
			.stdout_str();

		// Sanity check and sanitize fields.

		let description = read_field("%{DESCRIPTION}")?;

		let summary = if let Some(summary) = read_field("%{SUMMARY}")? {
			summary
		} else {
			// Older rpms will have no summary, but will have a description.
			// We'll take the 1st line out of the description, and use it for the summary.
			let description = description.as_deref().unwrap_or("");
			let s = description
				.split_once('\n')
				.map(|t| t.0)
				.unwrap_or(description);
			if s.is_empty() {
				// Fallback.
				"Converted RPM package".into()
			} else {
				s.to_owned()
			}
		};

		let description = description.unwrap_or_else(|| summary.clone());

		// Older rpms have no license tag, but have a copyright.
		let copyright = match read_field("%{LICENSE}")? {
			Some(o) => o,
			None => read_field("%{COPYRIGHT}")?.unwrap_or_else(|| "unknown".into()),
		};

		let Some(name) = read_field("%{NAME}")? else {
			bail!("Error querying rpm file: name not found!")
		};
		let Some(version) = read_field("%{VERSION}")? else {
			bail!("Error querying rpm file: version not found!")
		};
		let Some(release) = read_field("%{RELEASE}")?
			.and_then(|s| s.parse().ok())
		else {
			bail!("Error querying rpm file: release not found or invalid!")
		};

		let arch = if let Some(arch) = &args.target {
			let arch = match arch.as_bytes() {
				// NOTE(pluie): do NOT ask me where these numbers came from.
				// I have NO clue.
				b"1" => "i386",
				b"2" => "alpha",
				b"3" => "sparc",
				b"6" => "m68k",
				b"noarch" => "all",
				b"ppc" => "powerpc",
				b"x86_64" | b"em64t" => "amd64",
				b"armv4l" => "arm",
				b"armv7l" => "armel",
				b"parisc" => "hppa",
				b"ppc64le" => "ppc64el",

				// Treat 486, 586, etc, and Pentium, as 386.
				o if o.eq_ignore_ascii_case(b"pentium") => "i386",
				&[b'i' | b'I', b'0'..=b'9', b'8', b'6'] => "i386",

				_ => arch,
			};
			arch.to_owned()
		} else {
			read_field("%{ARCH}")?.unwrap_or_default()
		};

		let info = PackageInfo {
			name,
			version,
			release,
			arch,
			changelog_text: read_field("%{CHANGELOGTEXT}")?.unwrap_or_default(),
			summary,
			description,
			preinst: sanitize_script(read_field("%{PREIN}")?),
			postinst: sanitize_script(read_field("%{POSTIN}")?),
			prerm: sanitize_script(read_field("%{PREUN}")?),
			postrm: sanitize_script(read_field("%{POSTUN}")?),
			copyright,

			conffiles,
			file_list,
			binary_info,

			distribution: "Red Hat".into(),
			original_format: Format::Rpm,
			..Default::default()
		};

		Ok(Self {
			info,
			rpm_file,
			prefixes,
		})
	}
	pub(crate) fn build_with(&mut self, cmd: &str, unpacked_dir: &Path) -> Result<PathBuf> {
		let rpmdir = Exec::cmd("rpm")
			.arg("--showrc")
			.log_and_output(None)?
			.stdout_str()
			.lines()
			.find_map(|l| {
				if let Some(l) = l.strip_prefix("rpmdir") {
					let path = l.trim_start().trim_start_matches(':').trim_start();
					Some(PathBuf::from(path))
				} else {
					None
				}
			});

		let PackageInfo {
			name,
			version,
			release,
			arch,
			..
		} = &self.info;

		let rpm = format!("{name}-{version}-{release}.{arch}.rpm");

		let (rpm, arch_flag) = if let Some(rpmdir) = rpmdir {
			// Old versions of rpm toss it off in te middle of nowhere.
			let mut r = rpmdir.join(arch);
			r.push(&rpm);
			(r, "--buildarch")
		} else {
			// Presumably we're dealing with rpm 3.0 or above, which doesn't
			// output rpmdir in any format I'd care to try to parse.
			// Instead, rpm is now of a late enough version to notice the
			// %define's in the spec file, which will make the file end up
			// in the directory we started in.
			// Anyway, let's assume this is version 3 or above.

			// This is the new command line argument to set the arch rpms.
			// It appeared in rpm version 3.
			(PathBuf::from(rpm), "--target")
		};

		let mut build_root = std::env::current_dir()?;
		build_root.push(unpacked_dir);

		let mut cmd = Exec::cmd(cmd)
			.cwd(unpacked_dir)
			.stderr(Redirection::Merge)
			.arg("--buildroot")
			.arg(build_root)
			.arg("-bb")
			.arg(arch_flag)
			.arg(arch);

		if let Ok(opt) = std::env::var("RPMBUILDOPT") {
			let opt: Vec<_> = opt.split(' ').collect();
			cmd = cmd.args(&opt);
		}

		let spec = format!("{name}-{version}-{release}.spec");

		let cmdline = cmd.to_cmdline_lossy();
		let out = cmd.arg(&spec).log_and_output_without_checking(None)?;

		if !out.success() {
			bail!(
				"Package build failed. Here's the log of the command ({cmdline}):\n{}",
				out.stdout_str()
			);
		}

		Ok(rpm)
	}
}

#[test]
fn hmmm() {
	let a = std::env::var("LS_ARGS").unwrap();
	let a = a.split(" ").collect::<Vec<_>>();
	dbg!(Exec::cmd("ls").args(&a).capture().unwrap().stdout_str());
}

impl PackageBehavior for Rpm {
	fn info(&self) -> &PackageInfo {
		&self.info
	}
	fn info_mut(&mut self) -> &mut PackageInfo {
		&mut self.info
	}

	fn install(&mut self, file_name: &Path) -> Result<()> {
		let cmd = Exec::cmd("rpm").arg("-ivh");
		let cmd = if let Some(opt) = std::env::var_os("RPMINSTALLOPT") {
			let mut path = PathBuf::from(opt);
			path.push(file_name);
			cmd.arg(path)
		} else {
			cmd.arg(file_name)
		};
		cmd.log_and_output(Verbosity::VeryVerbose)
			.wrap_err("Unable to install")?;
		Ok(())
	}
	fn unpack(&mut self) -> Result<PathBuf> {
		let work_dir = common::make_unpack_work_dir(&self.info)?;

		let rpm2cpio = || Exec::cmd("rpm2cpio").arg(&self.rpm_file);

		// Check if we need to use lzma to uncompress the cpio archive
		let cmd = rpm2cpio()
			| Exec::cmd("lzma")
				.arg("-tq")
				.stdout(NullFile)
				.stderr(NullFile);

		let decomp = if cmd.log_and_output(None)?.success() {
			|| Exec::cmd("lzma").arg("-dq")
		} else {
			|| Exec::cmd("cat")
		};

		let cpio = Exec::cmd("cpio")
			.cwd(&work_dir)
			.args(&[
				"--extract",
				"--make-directories",
				"--no-absolute-filenames",
				"--preserve-modification-time",
			])
			.stderr(Redirection::Merge);

		(rpm2cpio() | decomp() | cpio)
			.log_and_spawn(None)
			.wrap_err_with(|| format!("Unpacking of {} failed", self.rpm_file.display()))?;

		// `cpio` does not necessarily store all parent directories in an archive,
		// and so some directories, if it has to make them and has no permission info,
		// will come out with some random permissions.
		// Find those directories and make them mode 755, which is more reasonable.

		let cpio = Exec::cmd("cpio").args(&["-it", "--quiet"]);
		let seen_files: HashSet<_> = (rpm2cpio() | decomp() | cpio)
			.log_and_output(None)
			.wrap_err_with(|| format!("File list of {} failed", self.rpm_file.display()))?
			.stdout_str()
			.lines()
			.map(PathBuf::from)
			.collect();

		let cwd = std::env::current_dir()?;
		std::env::set_current_dir(&work_dir)?;
		// glob doesn't allow you to specify a cwd... annoying, but ok
		for file in glob::glob("**/*").unwrap() {
			let file = file?;
			let new_file = work_dir.join(&file);
			if !seen_files.contains(&file) && new_file.exists() && !new_file.is_symlink() {
				chmod(&new_file, 0o755)?;
			}
		}
		std::env::set_current_dir(cwd)?;

		// If the package is relocatable, we'd like to move it to be under the `self.prefixes` directory.
		// However, it's possible that that directory is in the package - it seems some rpm's are marked
		// as relocatable and unpack already in the directory they can relocate to, while some are marked
		// relocatable and the directory they can relocate to is removed from all filenames in the package.
		// I suppose this is due to some change between versions of rpm, but none of this is adequately documented,
		// so we'll just muddle through.

		if let Some(prefixes) = &self.prefixes {
			let w_prefixes = work_dir.join(prefixes);
			if !w_prefixes.exists() {
				let mut relocate = true;

				// Get the files to move.
				let pattern = work_dir.join("*");
				let file_list: Vec<_> = glob::glob(&pattern.to_string_lossy())
					.unwrap()
					.filter_map(|p| p.ok())
					.collect();

				// Now, make the destination directory.
				let mut dest = PathBuf::new();

				for comp in prefixes.components() {
					if comp == Component::CurDir {
						dest.push("/");
					}
					dest.push(comp);

					if dest.is_dir() {
						// The package contains a parent directory of the relocation directory.
						// Since it's impossible to move a parent directory into its child,
						// bail out and do nothing.
						relocate = false;
						break;
					}
					std::fs::create_dir(&dest)?;
				}

				if relocate {
					// Now move all files in the package to the directory we made.
					if !file_list.is_empty() {
						fs_extra::move_items(&file_list, &w_prefixes, &CopyOptions::new())?;
					}

					self.info.conffiles = self
						.info
						.conffiles
						.iter()
						.map(|f| prefixes.join(f))
						.collect();
				}
			}
		}

		// `rpm` files have two sets of permissions; the set in the cpio archive,
		// and the set in the control data, which override the set in the archive.
		// The set in the control data are more correct, so let's use those.
		// Some permissions setting may have to be postponed until the postinst.

		let out = Exec::cmd("rpm")
			.args(&[
				"--queryformat",
				r#"'[%{FILEMODES} %{FILEUSERNAME} %{FILEGROUPNAME} %{FILENAMES}\n]'"#,
				"-qp",
			])
			.arg(&self.rpm_file)
			.log_and_output(None)?
			.stdout_str();

		let mut owninfo = HashMap::new();
		let mut modeinfo = HashMap::new();

		for line in out.lines() {
			let mut line = line.split(' ');
			let Some(mode) = line.next() else { continue; };
			let Some(owner) = line.next() else { continue; };
			let Some(group) = line.next() else { continue; };
			let Some(file) = line.next() else { continue; };

			let mut mode: u32 = mode.parse()?;
			mode &= 0o7777; // remove filetype

			let file = PathBuf::from(file);

			// TODO: this is not gonna work on windows, is it
			let uid = match User::from_name(owner)? {
				Some(User { uid, .. }) if uid.is_root() => uid,
				_ => {
					owninfo.insert(file.clone(), owner.to_owned());
					Uid::from_raw(0)
				}
			};
			let gid = match Group::from_name(group)? {
				Some(Group { gid, .. }) if gid.as_raw() == 0 => gid,
				_ => {
					let s = owninfo.entry(file.clone()).or_default();
					s.push(':');
					s.push_str(group);
					Gid::from_raw(0)
				}
			};

			if owninfo.contains_key(&file) && mode & 0o7000 > 0 {
				modeinfo.insert(file.clone(), mode);
			}

			// Note that ghost files exist in the metadata but not in the cpio archive,
			// so check that the file exists before trying to access it.
			let file = work_dir.join(file);
			if file.exists() {
				if geteuid().is_root() {
					chown(&file, Some(uid), Some(gid)).wrap_err_with(|| {
						format!("failed chowning {} to {uid}:{gid}", file.display())
					})?;
				}
				chmod(&file, mode)
					.wrap_err_with(|| format!("failed chowning {} to {mode}", file.display()))?;
			}
		}
		self.info.owninfo = owninfo;
		self.info.modeinfo = modeinfo;

		Ok(work_dir)
	}

	fn prepare(&mut self, unpacked_dir: &Path) -> Result<()> {
		let mut file_list = String::new();
		for filename in &self.info.file_list {
			// DIFFERENCE WITH THE PERL VERSION:
			// `snailquote` doesn't escape the same characters as Perl, but that difference
			// is negligible at best - feel free to implement Perl-style escaping if you want to.
			// The list of escape sequences is in `perlop`.

			// Unquote any escaped characters in filenames - needed for non ascii characters.
			// (eg. iso_8859-1 latin set)
			let unquoted = snailquote::unescape(&filename.to_string_lossy())?;

			if unquoted.ends_with('/') {
				file_list.push_str("%dir ");
			} else if self
				.info
				.conffiles
				.iter()
				.any(|f| f.as_os_str() == unquoted.as_str())
			{
				// it's a conffile
				file_list.push_str("%config ");
			}
			// Note all filenames are quoted in case they contain spaces.
			writeln!(file_list, r#""{unquoted}""#)?;
		}

		let PackageInfo {
			name,
			version,
			release,
			depends,
			summary,
			copyright,
			distribution,
			group,
			use_scripts,
			preinst,
			postinst,
			prerm,
			postrm,
			description,
			original_format,
			..
		} = &self.info;

		let mut spec = File::create(format!(
			"{}/{name}-{version}-{release}.spec",
			unpacked_dir.display()
		))?;

		let mut build_root = std::env::current_dir()?;
		build_root.push(unpacked_dir);

		#[rustfmt::skip]
		write!(
			spec,
r#"Buildroot: {build_root}
Name: {name}
Version: {version}
Release: {release}
"#,
			build_root = build_root.display(),
		)?;

		if let [first, rest @ ..] = &depends[..] {
			write!(spec, "Requires: {first}",)?;
			for dep in rest {
				write!(spec, ", {dep}")?;
			}
			writeln!(spec)?;
		}

		#[rustfmt::skip]
		write!(
			spec,
r#"Summary: {summary}
License: {copyright}
Distribution: {distribution}
Group: Converted/{group}

%define _rpmdir ../
%define _rpmfilename %%{{NAME}}-%%{{VERSION}}-%%{{RELEASE}}.%%{{ARCH}}.rpm
%define _unpackaged_files_terminate_build 0

"#,
		)?;

		if *use_scripts {
			if let Some(preinst) = preinst {
				write!(spec, "%pre\n{preinst}\n\n")?;
			}
			if let Some(postinst) = postinst {
				write!(spec, "%pre\n{postinst}\n\n")?;
			}
			if let Some(prerm) = prerm {
				write!(spec, "%pre\n{prerm}\n\n")?;
			}
			if let Some(postrm) = postrm {
				write!(spec, "%pre\n{postrm}\n\n")?;
			}
		}
		#[rustfmt::skip]
		write!(
			spec,
r#"%description
{description}

(Converted from a {original_format} package by alien version {alien_version}.)

%files
{file_list}"#,
			alien_version = env!("CARGO_PKG_VERSION")
		)?;

		Ok(())
	}

	fn sanitize_info(&mut self) -> Result<()> {
		self.info.version = self.info.version.replace('-', "_");

		// When retrieving scripts for building, we have to do some truly sick mangling.
		// Since debian/slackware scripts can be anything -- perl programs or binary files --
		// and rpm is limited to only shell scripts, we need to encode the files and add a
		// scrap of shell script to make it unextract and run on the fly.

		fn script_helper(script: &mut Option<String>) {
			let Some(s) = script.as_mut() else { return; };

			if s.chars().all(char::is_whitespace) {
				return; // it's blank.
			}

			if let Some(s) = s.strip_prefix("#!") {
				if s.trim_start().starts_with("/bin/sh") {
					return; // looks like a shell script already
				}
			}
			// The original used uuencoding. That is cursed. We don't do that here
			let encoded = base64::engine::general_purpose::STANDARD.encode(&s);

			#[rustfmt::skip]
			let patched = format!(
r#"#!/bin/sh
set -e
mkdir /tmp/alien.$$
echo '{encoded}' | base64 -d > /tmp/alien.$$/script
chmod 755 /tmp/alien.$$/script
/tmp/alien.$$/script "$@"
rm -f /tmp/alien.$$/script
rmdir /tmp/alien.$$
"#
			);
			*script = Some(patched);
		}

		script_helper(&mut self.info.preinst);
		script_helper(&mut self.info.postinst);
		script_helper(&mut self.info.prerm);
		script_helper(&mut self.info.postrm);

		let arch = match self.info.arch.as_str() {
			"amd64" => Some("x86_64"),
			"powerpc" => Some("ppc"), // XXX is this the canonical name for powerpc on rpm systems?
			"hppa" => Some("parisc"),
			"all" => Some("noarch"),
			"ppc64el" => Some("ppc64le"),
			_ => None,
		};
		if let Some(arch) = arch {
			self.info.arch = arch.to_owned();
		}

		Ok(())
	}

	fn build(&mut self, unpacked_dir: &Path) -> Result<PathBuf> {
		self.build_with("rpmbuild", unpacked_dir)
	}
}