//! Repository-aware visibility and bounded symbolic-macro visibility grants.
//! Package-group labels are retained for fallible analysis-time resolution.
use crate::label::{Label, PackageId};
use crate::repository::RepositorySet;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Visibility {
    Public,
    #[default]
    Private,
    Labels(Vec<Label>),
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
            .map(|value| repositories.parse_deferred(value, &package.repository, &package.package))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_labels(labels)
    }

    /// RuleVisibility.validateAndSimplify (packages/RuleVisibility.java:162-215):
    /// `//visibility:public` subsumes every other label, `//visibility:private`
    /// is dropped next to other labels, an empty or all-private list is private,
    /// and any other `//visibility:X` except `__pkg__`/`__subpackages__` is a
    /// misspelling error.
    pub(crate) fn from_labels(labels: Vec<Label>) -> Result<Self, String> {
        let special = |label: &Label, name: &str| {
            label.repository.is_empty() && label.package == "visibility" && label.name == name
        };
        let mut public = false;
        let mut private = 0;
        for label in &labels {
            if special(label, "public") {
                public = true;
            } else if special(label, "private") {
                private += 1;
            } else if label.repository.is_empty()
                && label.package == "visibility"
                && !matches!(label.name.as_str(), "__pkg__" | "__subpackages__")
            {
                return Err(format!(
                    "Invalid visibility label '{label}'; did you mean //visibility:public or //visibility:private?"
                ));
            }
        }
        if public {
            return Ok(Self::Public);
        }
        if private == labels.len() {
            return Ok(Self::Private);
        }
        Ok(Self::Labels(
            labels
                .into_iter()
                .filter(|label| !special(label, "private"))
                .collect(),
        ))
    }

    /// Add an exact authority grant without changing the declared scope sequence.
    pub(crate) fn with_package(&self, package: &PackageId) -> Self {
        let exact = Label {
            repository: package.repository.clone(),
            package: package.package.clone(),
            name: "__pkg__".into(),
        };
        match self {
            Self::Public => Self::Public,
            Self::Private => Self::Labels(vec![exact]),
            Self::Labels(scopes) => {
                let mut scopes = scopes.clone();
                if !scopes.contains(&exact) {
                    scopes.push(exact);
                }
                Self::Labels(scopes)
            }
        }
    }

    /// Recover declared labels, including their order and duplicate scopes.
    pub(crate) fn declared_labels(&self) -> Vec<Label> {
        match self {
            Self::Public | Self::Private => vec![Label {
                repository: String::new(),
                package: "visibility".into(),
                name: if matches!(self, Self::Public) {
                    "public"
                } else {
                    "private"
                }
                .into(),
            }],
            Self::Labels(labels) => labels.clone(),
        }
    }

    pub(crate) fn group_labels(&self) -> impl Iterator<Item = &Label> {
        let labels = match self {
            Self::Labels(labels) => labels.as_slice(),
            Self::Public | Self::Private => &[],
        };
        labels
            .iter()
            .filter(|label| !matches!(label.name.as_str(), "__pkg__" | "__subpackages__"))
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
            Self::Labels(scopes) => scopes.iter().any(|s| {
                matches!(s.name.as_str(), "__pkg__" | "__subpackages__")
                    && requesting_package.repository == s.repository
                    && (requesting_package.package == s.package
                        || (s.name == "__subpackages__"
                            && (s.package.is_empty()
                                || requesting_package
                                    .package
                                    .starts_with(&format!("{}/", s.package)))))
            }),
        }
    }
}

/// Normalize the observable macro callback labels separately from ordinary
/// visibility parsing, whose mixed-sentinel rejection remains unchanged.
pub(crate) fn macro_visibility_labels(
    labels: Vec<Label>,
    caller: &PackageId,
) -> Result<Vec<Label>, String> {
    let mut public = false;
    let mut normalized = Vec::new();
    for label in labels {
        if label.repository.is_empty()
            && label.package == "visibility"
            && matches!(label.name.as_str(), "public" | "private")
        {
            public |= label.name == "public";
        } else {
            normalized.push(label);
        }
    }
    // Every input is validated before public can collapse the list.
    if public {
        return Ok(Visibility::Public.declared_labels());
    }
    let caller = Label {
        repository: caller.repository.clone(),
        package: caller.package.clone(),
        name: "__pkg__".into(),
    };
    if !normalized.contains(&caller) {
        normalized.push(caller);
    }
    // Label's derived ordering is canonical (repository, package, name).
    // Do not deduplicate: duplicates are observable callback data.
    normalized.sort();
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn labels(values: &[&str]) -> Vec<Label> {
        values
            .iter()
            .map(|value| Label::parse_canonical(value).unwrap())
            .collect()
    }

    #[test]
    fn macro_public_collapse_validates_every_label() {
        let caller = labels(&["//app:x"])[0].package_id();
        assert_eq!(
            macro_visibility_labels(
                labels(&[
                    "//visibility:private",
                    "//visibility:public",
                    "//z:__pkg__",
                    "//visibility:public",
                ]),
                &caller,
            )
            .unwrap(),
            labels(&["//visibility:public"])
        );
        for group in [
            "//x:group",
            "@@dep//visibility:public",
            "//visibility:publik",
        ] {
            for input in [
                vec!["//visibility:public", group],
                vec![group, "//visibility:public"],
            ] {
                assert_eq!(
                    macro_visibility_labels(labels(&input), &caller).unwrap(),
                    labels(&["//visibility:public"])
                );
            }
        }
    }

    #[test]
    fn macro_private_filter_preserves_duplicates_and_sorts_canonically() {
        let caller = labels(&["//app:x"])[0].package_id();
        assert_eq!(
            macro_visibility_labels(
                labels(&[
                    "//z:__pkg__",
                    "//visibility:private",
                    "//app:__pkg__",
                    "//z:__pkg__",
                    "//app:__pkg__",
                ]),
                &caller,
            )
            .unwrap(),
            labels(&[
                "//app:__pkg__",
                "//app:__pkg__",
                "//z:__pkg__",
                "//z:__pkg__",
            ])
        );
        for input in [
            vec![],
            vec!["//visibility:private"],
            vec!["//visibility:private", "//visibility:private"],
        ] {
            assert_eq!(
                macro_visibility_labels(labels(&input), &caller).unwrap(),
                labels(&["//app:__pkg__"])
            );
        }
        let external = labels(&["@@dep//app:x"])[0].package_id();
        assert_eq!(
            macro_visibility_labels(
                labels(&["@@z//:__pkg__", "//app:__pkg__", "//:__subpackages__"]),
                &external,
            )
            .unwrap(),
            labels(&[
                "//:__subpackages__",
                "//app:__pkg__",
                "@@dep//app:__pkg__",
                "@@z//:__pkg__",
            ])
        );
        assert_eq!(
            macro_visibility_labels(labels(&["//:__subpackages__"]), &caller).unwrap(),
            labels(&["//:__subpackages__", "//app:__pkg__"])
        );
    }

    #[test]
    fn declared_labels_and_authority_grants_preserve_scope_sequence() {
        let input = labels(&[
            "@@dep//z:__pkg__",
            "//:__subpackages__",
            "@@dep//z:__pkg__",
            "//groups:g",
            "//groups:g",
        ]);
        let visibility = Visibility::from_labels(input.clone()).unwrap();
        assert_eq!(visibility.declared_labels(), input);
        let package = labels(&["//app:x"])[0].package_id();
        let mut expected = input;
        expected.push(labels(&["//app:__pkg__"]).remove(0));
        let granted = visibility.with_package(&package);
        assert_eq!(granted.declared_labels(), expected);
        assert_eq!(granted.with_package(&package), granted);
        assert_eq!(
            Visibility::Public.with_package(&package),
            Visibility::Public
        );
        assert_eq!(
            Visibility::Private.with_package(&package).declared_labels(),
            labels(&["//app:__pkg__"])
        );
        assert_eq!(
            Visibility::Private.declared_labels(),
            labels(&["//visibility:private"])
        );
    }

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
    fn groups_are_retained_but_mixed_public_and_unknown_repositories_are_rejected() {
        let group = parse(&["//other:group", "//other:group", "//x:__pkg__"]);
        assert_eq!(
            group.declared_labels(),
            labels(&["//other:group", "//other:group", "//x:__pkg__"])
        );
        assert_eq!(
            group.group_labels().cloned().collect::<Vec<_>>(),
            labels(&["//other:group", "//other:group"])
        );
        assert!(!group.allows("owner", "other"));
        assert!(group.allows("owner", "x"));
        // RuleVisibility.validateAndSimplify: public subsumes, private drops.
        assert_eq!(
            parse(&["//x:__pkg__", "//visibility:public"]),
            Visibility::Public
        );
        assert_eq!(
            parse(&[
                "//visibility:private",
                "//x:__pkg__",
                "//visibility:private"
            ])
            .declared_labels(),
            labels(&["//x:__pkg__"])
        );
        assert_eq!(
            parse(&["//visibility:private", "//visibility:private"]),
            Visibility::Private
        );
        assert_eq!(
            parse(&["//visibility:__pkg__"]).declared_labels(),
            labels(&["//visibility:__pkg__"])
        );
        for (values, expected) in [
            (vec!["@repo//p:__pkg__"], "external"),
            (
                vec!["//x:__pkg__", "//visibility:pubic"],
                "Invalid visibility label '//visibility:pubic'; did you mean //visibility:public or //visibility:private?",
            ),
        ] {
            let error = Visibility::parse(
                &values.into_iter().map(String::from).collect::<Vec<_>>(),
                "owner",
            )
            .unwrap_err();
            assert!(error.contains(expected), "{error}");
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
            let visibility = Visibility::parse_in(&[input.into()], &owner, &repositories).unwrap();
            assert_eq!(
                visibility.group_labels().cloned().collect::<Vec<_>>(),
                labels(&[input])
            );
            assert!(!visibility.allows_in(&owner, &main_package()));
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

    fn main_package() -> PackageId {
        labels(&["//elsewhere:x"])[0].package_id()
    }
}
