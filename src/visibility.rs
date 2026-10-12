//! Repository-aware visibility; package groups and symbolic macro visibility are
//! deliberately not approximated.
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    Public,
    #[default]
    Private,
    Packages(Vec<PackageScope>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageScope {
    package: PackageId,
    recursive: bool,
}

impl Visibility {
    pub fn parse(values: &[String], package: &str) -> Result<Self, String> {
        let labels = values
            .iter()
            .map(|v| Label::parse(v, package))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_labels(labels)
    }

    pub fn parse_in(
        values: &[String],
        package: &PackageId,
        repositories: &RepositorySet,
    ) -> Result<Self, String> {
        let labels = values
            .iter()
            .map(|value| repositories.parse(value, &package.repository, &package.package))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_labels(labels)
    }

    fn from_labels(labels: Vec<Label>) -> Result<Self, String> {
        if labels.is_empty() {
            return Ok(Self::Private);
        }
        for label in &labels {
            if label.repository.is_empty()
                && label.package == "visibility"
                && matches!(label.name.as_str(), "public" | "private")
            {
                if labels.len() != 1 {
                    return Err(
                        "public/private visibility cannot be combined with other visibility labels"
                            .into(),
                    );
                }
                return Ok(if label.name == "public" {
                    Self::Public
                } else {
                    Self::Private
                });
            }
        }
        let mut scopes = Vec::new();
        for label in labels {
            let recursive = match label.name.as_str() {
                "__pkg__" => false,
                "__subpackages__" => true,
                _ => {
                    return Err(format!(
                        "package_group visibility is not implemented: {label}"
                    ));
                }
            };
            scopes.push(PackageScope {
                package: label.package_id(),
                recursive,
            });
        }
        Ok(Self::Packages(scopes))
    }

    pub fn allows(&self, declaring_package: &str, requesting_package: &str) -> bool {
        self.allows_in(
            &PackageId {
                repository: String::new(),
                package: declaring_package.to_owned(),
            },
            &PackageId {
                repository: String::new(),
                package: requesting_package.to_owned(),
            },
        )
    }

    pub fn allows_in(&self, declaring_package: &PackageId, requesting_package: &PackageId) -> bool {
        if declaring_package == requesting_package {
            return true;
        }
        match self {
            Self::Public => true,
            Self::Private => false,
            Self::Packages(scopes) => scopes.iter().any(|s| {
                requesting_package.repository == s.package.repository
                    && (requesting_package.package == s.package.package
                        || (s.recursive
                            && (s.package.package.is_empty()
                                || requesting_package
                                    .package
                                    .starts_with(&format!("{}/", s.package.package)))))
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(values: &[&str]) -> Visibility {
        Visibility::parse(
            &values.iter().map(|v| v.to_string()).collect::<Vec<_>>(),
            "owner",
        )
        .unwrap()
    }
    #[test]
    fn public_private_and_empty() {
        assert!(parse(&["//visibility:public"]).allows("owner", "other"));
        for v in [parse(&[]), parse(&["//visibility:private"])] {
            assert!(v.allows("owner", "owner"));
            assert!(!v.allows("owner", "other"));
        }
    }
    #[test]
    fn package_scopes_obey_component_boundaries() {
        let v = parse(&["//team:__subpackages__", "//single:__pkg__"]);
        for pkg in ["owner", "team", "team/a/b", "single"] {
            assert!(v.allows("owner", pkg));
        }
        for pkg in ["teams", "team-other", "single/child", ""] {
            assert!(!v.allows("owner", pkg));
        }
        assert!(parse(&["//:__subpackages__"]).allows("owner", "any/nested/package"));
        assert!(!parse(&[":__pkg__"]).allows("owner", "owner/child"));
    }
    #[test]
    fn groups_and_mixed_public_are_rejected() {
        for values in [
            vec!["//other:group"],
            vec!["//visibility:public", "//x:__pkg__"],
            vec!["@repo//p:__pkg__"],
        ] {
            assert!(
                Visibility::parse(
                    &values.into_iter().map(String::from).collect::<Vec<_>>(),
                    "owner"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn repositories_are_part_of_visibility_identity() {
        let repositories = RepositorySet::new("unused-no-module".into());
        let owner = Label::parse_canonical("@@dep+//owner:x")
            .unwrap()
            .package_id();
        let main_owner = Label::parse("//owner:x", "").unwrap().package_id();
        assert!(!Visibility::Private.allows_in(&owner, &main_owner));
        let visibility = Visibility::parse_in(
            &["//team:__subpackages__".into(), "@@//single:__pkg__".into()],
            &owner,
            &repositories,
        )
        .unwrap();
        for input in ["@@dep+//team:x", "@@dep+//team/a:x", "//single:x"] {
            assert!(
                visibility.allows_in(&owner, &Label::parse_canonical(input).unwrap().package_id())
            );
        }
        for input in [
            "//team:x",
            "@@other+//team/a:x",
            "@@dep+//teams:x",
            "@@dep+//single:x",
        ] {
            assert!(
                !visibility.allows_in(&owner, &Label::parse_canonical(input).unwrap().package_id())
            );
        }
    }

    #[test]
    fn public_sentinel_must_be_main_and_every_value_is_parsed() {
        let repositories = RepositorySet::new("unused-no-module".into());
        let owner = Label::parse_canonical("@@dep+//owner:x")
            .unwrap()
            .package_id();
        assert_eq!(
            Visibility::parse_in(&["//visibility:public".into()], &owner, &repositories).unwrap(),
            Visibility::Public
        );
        {
            let input = "@@dep+//visibility:public";
            assert!(
                Visibility::parse_in(&[input.into()], &owner, &repositories)
                    .unwrap_err()
                    .contains("package_group")
            );
        }
        assert!(
            Visibility::parse_in(
                &["//visibility:public".into(), "@unknown//p:__pkg__".into()],
                &owner,
                &repositories
            )
            .is_err()
        );
    }
}
