//! Pure, bounded planning for protection of identified interactive scopes.
//!
//! The caller discovers processes, verifies control ownership, and applies or
//! restores the resulting controls. This module never reads or writes cgroupfs.
//! Explicit ancestor budgets work with and without memory_recursiveprot; with
//! recursive protection enabled, unused protection can still reach siblings.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const MAX_SCOPES: usize = 1024;
const MAX_DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Role {
    Session,
    Input,
    Terminal,
    Remote,
}

impl Role {
    const fn cap_bytes(self) -> u64 {
        match self {
            Self::Session => 3 * GIB / 2,
            Self::Input => 64 * MIB,
            Self::Terminal | Self::Remote => 128 * MIB,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Scope {
    /// Absolute cgroup directory, not its memory.low control file.
    pub path: PathBuf,
    pub role: Role,
    pub current_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtectedLeaf {
    pub path: PathBuf,
    pub role: Role,
    pub current_bytes: u64,
    /// Page-aligned request before the shared budgets are applied.
    pub requested_bytes: u64,
    /// Page-aligned allocation; zero means the budget cannot cover this leaf.
    pub limit_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProtectionPlan {
    /// Absolute memory.low file paths. Ancestors carry their leaves' budgets;
    /// summing this map would count the same protection more than once.
    pub limits: BTreeMap<PathBuf, u64>,
    pub leaves: Vec<ProtectedLeaf>,
    /// Total ceiling: min(physical RAM / 8, 2 GiB), rounded down to pages.
    pub budget_bytes: u64,
    /// Sum of leaf allocations, without counting ancestors again.
    pub allocated_bytes: u64,
    /// Remaining user ceiling after the actual system allocation is deducted.
    pub user_budget_bytes: u64,
    /// Actual system allocation, also bounded by one eighth of the total budget.
    pub system_budget_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Category {
    App,
    Session,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Domain {
    System,
    User(u32, Category),
}

#[derive(Clone, Debug)]
struct Candidate {
    leaf: ProtectedLeaf,
    domain: Domain,
}

/// Build one complete plan, or reject the input without producing a partial one.
///
/// Duplicate paths use the largest bounded request, not the sum of roles. When
/// both an ancestor and a descendant were selected, only the deepest selected
/// scopes receive leaf budgets; their ancestors carry those budgets once.
/// System services share at most B/8. The remaining user ceiling is divided
/// equally between users that have nonzero requests; each user's session/app
/// ceilings retain the existing 3/4 and 1/4 split. Unused shares are not assigned
/// to whole slices or redistributed to unrelated scopes.
pub fn plan_protection(
    cgroup_root: &Path,
    total_bytes: u64,
    page_size: u64,
    scopes: &[Scope],
) -> Result<ProtectionPlan, String> {
    if total_bytes == 0 {
        return Err("protection planning requires nonzero physical memory".into());
    }
    if !page_size.is_power_of_two() {
        return Err("protection planning requires a nonzero power-of-two page size".into());
    }
    if scopes.len() > MAX_SCOPES {
        return Err("too many protection candidates; preserve the preceding complete plan".into());
    }
    let root_parts = absolute_parts(cgroup_root)?;
    let budget_pages = (total_bytes / 8).min(2 * GIB) / page_size;
    let mut unique: BTreeMap<PathBuf, Candidate> = BTreeMap::new();
    for scope in scopes {
        let domain = classify(cgroup_root, &root_parts, scope)?;
        let requested = scope.current_bytes.min(scope.role.cap_bytes()) / page_size * page_size;
        if requested == 0 {
            continue;
        }
        let candidate = Candidate {
            leaf: ProtectedLeaf {
                path: scope.path.clone(),
                role: scope.role,
                current_bytes: scope.current_bytes,
                requested_bytes: requested,
                limit_bytes: 0,
            },
            domain,
        };
        match unique.get_mut(&scope.path) {
            Some(previous) => {
                // A deterministic tie-break keeps discovery order irrelevant.
                if preference(&candidate.leaf) > preference(&previous.leaf) {
                    *previous = candidate;
                }
            }
            None => {
                unique.insert(scope.path.clone(), candidate);
            }
        }
    }

    let mut shadowed = BTreeSet::new();
    for path in unique.keys() {
        for ancestor in path.ancestors().skip(1) {
            if ancestor == cgroup_root {
                break;
            }
            if unique.contains_key(ancestor) {
                shadowed.insert(ancestor.to_path_buf());
            }
        }
    }
    let mut candidates: Vec<_> = unique
        .into_values()
        .filter(|candidate| !shadowed.contains(&candidate.leaf.path))
        .collect();
    let mut system = Vec::new();
    let mut users: BTreeMap<u32, (Vec<usize>, Vec<usize>)> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        match candidate.domain {
            Domain::System => system.push(index),
            Domain::User(uid, category) => {
                let (session, app) = users.entry(uid).or_default();
                match category {
                    Category::Session => session.push(index),
                    Category::App => app.push(index),
                }
            }
        }
    }
    let system_pages = allocate(&mut candidates, &system, budget_pages / 8, page_size);
    let user_pages = budget_pages - system_pages;
    let per_user = user_pages / users.len().max(1) as u64;
    for (session, app) in users.into_values() {
        let session_pages = per_user * 3 / 4;
        allocate(&mut candidates, &session, session_pages, page_size);
        allocate(&mut candidates, &app, per_user - session_pages, page_size);
    }

    let mut limits = BTreeMap::new();
    let mut allocated_bytes = 0;
    let leaves: Vec<_> = candidates
        .into_iter()
        .map(|candidate| candidate.leaf)
        .collect();
    for leaf in &leaves {
        if leaf.limit_bytes == 0 {
            continue;
        }
        allocated_bytes += leaf.limit_bytes;
        for ancestor in leaf.path.ancestors() {
            if ancestor == cgroup_root {
                break;
            }
            *limits.entry(ancestor.join("memory.low")).or_insert(0) += leaf.limit_bytes;
        }
    }
    Ok(ProtectionPlan {
        limits,
        leaves,
        budget_bytes: budget_pages * page_size,
        allocated_bytes,
        user_budget_bytes: user_pages * page_size,
        system_budget_bytes: system_pages * page_size,
    })
}

fn preference(leaf: &ProtectedLeaf) -> (u64, u64, u64, Role) {
    (
        leaf.requested_bytes,
        leaf.role.cap_bytes(),
        leaf.current_bytes,
        leaf.role,
    )
}

/// Allocate in pages so rounding cannot exceed either the request or the cap.
/// Proportional rounding may leave less than one page per candidate unused.
fn allocate(
    candidates: &mut [Candidate],
    indices: &[usize],
    cap_pages: u64,
    page_size: u64,
) -> u64 {
    let requested: u128 = indices
        .iter()
        .map(|&index| (candidates[index].leaf.requested_bytes / page_size) as u128)
        .sum();
    let mut allocated = 0;
    for &index in indices {
        let leaf = &mut candidates[index].leaf;
        let pages = leaf.requested_bytes / page_size;
        let pages = if requested <= cap_pages as u128 {
            pages
        } else {
            (pages as u128 * cap_pages as u128 / requested) as u64
        };
        leaf.limit_bytes = pages * page_size;
        allocated += pages;
    }
    allocated
}

fn absolute_parts(path: &Path) -> Result<Vec<&str>, String> {
    let text = path
        .to_str()
        .ok_or_else(|| format!("non-UTF-8 protection path: {}", path.display()))?;
    let tail = text
        .strip_prefix('/')
        .ok_or_else(|| format!("protection path must be absolute: {}", path.display()))?;
    let parts: Vec<_> = tail.split('/').collect();
    if parts.iter().any(|part| {
        part.is_empty()
            || *part == "."
            || *part == ".."
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._@:-\\".contains(&b))
    }) {
        return Err(format!("noncanonical protection path: {}", path.display()));
    }
    Ok(parts)
}

fn uid_component(component: &str) -> Option<u32> {
    let text = component.strip_prefix("user-")?.strip_suffix(".slice")?;
    let uid = text.parse::<u32>().ok()?;
    (text == uid.to_string()).then_some(uid)
}

fn classify(root: &Path, root_parts: &[&str], scope: &Scope) -> Result<Domain, String> {
    let parts = absolute_parts(&scope.path)?;
    let reject = || format!("unsupported protection scope: {}", scope.path.display());
    if !parts.starts_with(root_parts) || parts.len() <= root_parts.len() {
        return Err(reject());
    }
    let tail = &parts[root_parts.len()..];
    if tail.len() > MAX_DEPTH || !scope.path.starts_with(root) {
        return Err(reject());
    }
    if tail.len() == 2
        && tail[0] == "system.slice"
        && ["ssh.service", "sshd.service", "systemd-logind.service"].contains(&tail[1])
    {
        return Ok(Domain::System);
    }
    if tail.len() < 3 || tail[0] != "user.slice" {
        return Err(reject());
    }
    let uid = uid_component(tail[1]).ok_or_else(reject)?;
    if tail.len() == 3 && scope.role == Role::Remote {
        let session = tail[2]
            .strip_prefix("session-")
            .and_then(|v| v.strip_suffix(".scope"));
        if session.is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            return Ok(Domain::User(uid, Category::Session));
        }
    }
    if tail.len() < 5 || tail[2] != format!("user@{uid}.service") {
        return Err(reject());
    }
    let category = match tail[3] {
        "app.slice" => Category::App,
        "session.slice" => Category::Session,
        _ => return Err(reject()),
    };
    let named = |part: &str, suffix: &str| {
        part.strip_suffix(suffix)
            .is_some_and(|stem| !stem.is_empty())
    };
    let is_unit = |part: &str| named(part, ".scope") || named(part, ".service");
    if !is_unit(tail[tail.len() - 1])
        || tail[4..]
            .iter()
            .any(|part| !is_unit(part) && !named(part, ".slice"))
    {
        return Err(reject());
    }
    Ok(Domain::User(uid, category))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/sys/fs/cgroup";
    const PAGE: u64 = 4096;

    fn user(uid: u32, category: &str, unit: &str, role: Role, current: u64) -> Scope {
        Scope {
            path: Path::new(ROOT).join(format!(
                "user.slice/user-{uid}.slice/user@{uid}.service/{category}.slice/{unit}"
            )),
            role,
            current_bytes: current,
        }
    }

    fn system(unit: &str, role: Role, current: u64) -> Scope {
        Scope {
            path: Path::new(ROOT).join("system.slice").join(unit),
            role,
            current_bytes: current,
        }
    }

    fn plan(scopes: &[Scope]) -> ProtectionPlan {
        plan_protection(Path::new(ROOT), 16 * GIB, PAGE, scopes).unwrap()
    }

    fn low(plan: &ProtectionPlan, directory: &Path) -> u64 {
        plan.limits[&directory.join("memory.low")]
    }

    #[test]
    fn protection_plan_carries_only_leaf_budgets_through_every_ancestor() {
        let terminal = user(
            1000,
            "app",
            "app-graphical.slice/terminal.scope",
            Role::Terminal,
            GIB,
        );
        let input = user(
            1000,
            "app",
            "app-graphical.slice/input.scope",
            Role::Input,
            24 * MIB,
        );
        let wm = user(
            1000,
            "session",
            "wayland-wm.scope",
            Role::Session,
            512 * MIB,
        );
        let p = plan(&[terminal.clone(), input.clone(), wm.clone()]);
        let graphical = terminal.path.parent().unwrap();
        assert_eq!(low(&p, &terminal.path), 128 * MIB);
        assert_eq!(low(&p, &input.path), 24 * MIB);
        assert_eq!(low(&p, graphical), 152 * MIB);
        assert_eq!(low(&p, graphical.parent().unwrap()), 152 * MIB);
        assert_eq!(low(&p, &wm.path), 512 * MIB);
        assert_eq!(low(&p, wm.path.parent().unwrap()), 512 * MIB);
        for ancestor in wm.path.ancestors().skip(2) {
            if ancestor == Path::new(ROOT) {
                break;
            }
            assert_eq!(low(&p, ancestor), 664 * MIB);
        }
        assert_eq!(p.allocated_bytes, 664 * MIB);
        assert!(!p.limits.contains_key(&Path::new(ROOT).join("memory.low")));
        assert!(
            !p.limits
                .contains_key(&graphical.join("unrelated.scope/memory.low"))
        );
    }

    #[test]
    fn protection_plan_deduplicates_roles_and_is_independent_of_discovery_order() {
        let terminal = user(1000, "app", "terminal.scope", Role::Terminal, 256 * MIB);
        let mut same = terminal.clone();
        same.role = Role::Session;
        let p = plan(&[terminal.clone(), same.clone(), terminal.clone()]);
        assert_eq!(p, plan(&[same, terminal]));
        assert_eq!(p.leaves.len(), 1);
        assert_eq!(p.leaves[0].role, Role::Session);
        assert_eq!(p.allocated_bytes, 256 * MIB);
    }

    #[test]
    fn protection_plan_uses_deepest_selected_scopes_without_double_counting() {
        let parent = user(1000, "app", "terminal.service", Role::Session, GIB);
        let child = user(
            1000,
            "app",
            "terminal.service/worker.scope",
            Role::Terminal,
            96 * MIB,
        );
        let p = plan(&[parent.clone(), child.clone()]);
        assert_eq!(p.leaves.len(), 1);
        assert_eq!(p.leaves[0].path, child.path);
        assert_eq!(p.allocated_bytes, 96 * MIB);
        assert_eq!(low(&p, &parent.path), 96 * MIB);
    }

    #[test]
    fn protection_plan_zero_usage_child_does_not_erase_parent_request() {
        let parent = user(1000, "app", "terminal.service", Role::Terminal, 96 * MIB);
        let child = user(
            1000,
            "app",
            "terminal.service/worker.scope",
            Role::Terminal,
            0,
        );
        let p = plan(&[parent.clone(), child]);
        assert_eq!(p.leaves.len(), 1);
        assert_eq!(p.leaves[0].path, parent.path);
        assert_eq!(p.allocated_bytes, 96 * MIB);
    }

    #[test]
    fn protection_plan_does_not_confuse_name_prefix_with_ancestry() {
        let a = user(1000, "app", "terminal.scope", Role::Terminal, 64 * MIB);
        let b = user(
            1000,
            "app",
            "terminal.scope-extra.scope",
            Role::Terminal,
            64 * MIB,
        );
        let p = plan(&[a, b]);
        assert_eq!(p.leaves.len(), 2);
        assert_eq!(p.allocated_bytes, 128 * MIB);
    }

    #[test]
    fn protection_plan_rounds_down_for_both_kernel_page_sizes() {
        for page in [4096, 65536] {
            let scope = user(1000, "app", "input.scope", Role::Input, 3 * page + 13);
            let p = plan_protection(Path::new(ROOT), 16 * GIB, page, &[scope]).unwrap();
            assert_eq!(p.leaves[0].requested_bytes, 3 * page);
            assert_eq!(p.leaves[0].limit_bytes, 3 * page);
            assert!(p.limits.values().all(|value| value % page == 0));
            assert_eq!(p.budget_bytes % page, 0);
        }
    }

    #[test]
    fn protection_plan_keeps_role_and_observed_usage_bounds() {
        let scopes = [
            user(1000, "session", "wm.scope", Role::Session, 4 * GIB),
            user(1000, "app", "input.scope", Role::Input, GIB),
            user(1000, "app", "terminal.scope", Role::Terminal, GIB),
            user(1000, "app", "remote.scope", Role::Remote, 20 * MIB + 1),
        ];
        let p = plan(&scopes);
        for leaf in &p.leaves {
            assert!(leaf.requested_bytes <= leaf.role.cap_bytes());
            assert!(leaf.limit_bytes <= leaf.requested_bytes);
            assert!(leaf.requested_bytes <= leaf.current_bytes);
        }
        let remote = p
            .leaves
            .iter()
            .find(|leaf| leaf.role == Role::Remote)
            .unwrap();
        assert_eq!(remote.limit_bytes, 20 * MIB);
    }

    #[test]
    fn protection_plan_preserves_session_three_quarters_and_app_one_quarter() {
        let mut scopes = vec![user(1000, "session", "wm.scope", Role::Session, 4 * GIB)];
        for index in 0..8 {
            scopes.push(user(
                1000,
                "app",
                &format!("terminal-{index}.scope"),
                Role::Terminal,
                GIB,
            ));
        }
        let p = plan_protection(Path::new(ROOT), 8 * GIB, PAGE, &scopes).unwrap();
        let manager = Path::new(ROOT).join("user.slice/user-1000.slice/user@1000.service");
        assert_eq!(low(&p, &manager.join("session.slice")), 768 * MIB);
        assert_eq!(low(&p, &manager.join("app.slice")), 256 * MIB);
        assert_eq!(p.allocated_bytes, GIB);
    }

    #[test]
    fn protection_plan_leaves_unused_categories_unallocated() {
        let scopes: Vec<_> = (0..8)
            .map(|index| {
                user(
                    1000,
                    "app",
                    &format!("term-{index}.scope"),
                    Role::Terminal,
                    GIB,
                )
            })
            .collect();
        let p = plan(&scopes);
        assert_eq!(p.allocated_bytes, 512 * MIB);
        assert_eq!(p.budget_bytes, 2 * GIB);
        assert!(
            !p.limits
                .keys()
                .any(|path| path.to_string_lossy().contains("session.slice"))
        );
    }

    #[test]
    fn protection_plan_shares_user_ceiling_without_lending_unused_user_shares() {
        let mut scopes: Vec<_> = (0..4)
            .map(|index| {
                user(
                    1000,
                    "app",
                    &format!("term-{index}.scope"),
                    Role::Terminal,
                    GIB,
                )
            })
            .collect();
        scopes.push(user(1001, "app", "term.scope", Role::Terminal, GIB));
        let p = plan(&scopes);
        let first = Path::new(ROOT).join("user.slice/user-1000.slice");
        let second = Path::new(ROOT).join("user.slice/user-1001.slice");
        assert_eq!(low(&p, &first), 256 * MIB);
        assert_eq!(low(&p, &second), 128 * MIB);
        assert_eq!(low(&p, &Path::new(ROOT).join("user.slice")), 384 * MIB);
    }

    #[test]
    fn protection_plan_system_allocations_are_capped_and_deducted_from_total() {
        let scopes = [
            system("sshd.service", Role::Remote, GIB),
            system("ssh.service", Role::Remote, GIB),
            system("systemd-logind.service", Role::Session, GIB),
            user(1000, "session", "wm.scope", Role::Session, 4 * GIB),
            user(1000, "app", "term.scope", Role::Terminal, GIB),
        ];
        let p = plan(&scopes);
        assert!(p.system_budget_bytes <= p.budget_bytes / 8);
        assert_eq!(p.user_budget_bytes + p.system_budget_bytes, p.budget_bytes);
        assert_eq!(
            low(&p, &Path::new(ROOT).join("system.slice")),
            p.system_budget_bytes
        );
        assert!(p.allocated_bytes <= p.budget_bytes);
        assert!(p.limits.values().all(|value| *value <= p.budget_bytes));
    }

    #[test]
    fn protection_plan_direct_login_session_requires_an_explicit_remote_role() {
        let mut scope = Scope {
            path: Path::new(ROOT).join("user.slice/user-1000.slice/session-4.scope"),
            role: Role::Remote,
            current_bytes: 48 * MIB,
        };
        assert_eq!(plan(std::slice::from_ref(&scope)).allocated_bytes, 48 * MIB);
        scope.path = Path::new(ROOT).join("user.slice/user-1000.slice/session-c1.scope");
        assert_eq!(plan(std::slice::from_ref(&scope)).allocated_bytes, 48 * MIB);
        scope.role = Role::Terminal;
        assert!(plan_protection(Path::new(ROOT), 16 * GIB, PAGE, &[scope]).is_err());
    }

    #[test]
    fn protection_plan_rejects_unsupported_paths_without_a_partial_plan() {
        for path in [
            "/sys/fs/cgroup",
            "/sys/fs/cgroup/user.slice",
            "/sys/fs/cgroup/system.slice/arbitrary.service",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1001.service/app.slice/term.scope",
            "/sys/fs/cgroup/user.slice/user-01000.slice/user@01000.service/app.slice/term.scope",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/zmem.slice/term.scope",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/app-graphical.slice",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/.scope",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/../term.scope",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/./term.scope",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice//term.scope",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/term.scope/",
            "/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/term\n.scope",
            "/sys/fs/cgroup-other/user.slice/user-1000.slice/user@1000.service/app.slice/term.scope",
            "user.slice/user-1000.slice/user@1000.service/app.slice/term.scope",
        ] {
            let scopes = [
                user(1000, "app", "valid.scope", Role::Terminal, MIB),
                Scope {
                    path: path.into(),
                    role: Role::Terminal,
                    current_bytes: MIB,
                },
            ];
            assert!(
                plan_protection(Path::new(ROOT), 16 * GIB, PAGE, &scopes).is_err(),
                "{path}"
            );
        }
    }

    #[test]
    fn protection_plan_rejects_invalid_dimensions_and_excessive_candidates() {
        for page in [0, 3, 4095] {
            assert!(plan_protection(Path::new(ROOT), 16 * GIB, page, &[]).is_err());
        }
        assert!(plan_protection(Path::new(ROOT), 0, PAGE, &[]).is_err());
        for root in ["/", "relative", "/sys/fs/../cgroup", "/sys/fs/cgroup/"] {
            assert!(plan_protection(Path::new(root), 16 * GIB, PAGE, &[]).is_err());
        }
        let scope = user(1000, "app", "term.scope", Role::Terminal, MIB);
        assert!(
            plan_protection(
                Path::new(ROOT),
                16 * GIB,
                PAGE,
                &vec![scope; MAX_SCOPES + 1]
            )
            .is_err()
        );
    }

    #[test]
    fn protection_plan_handles_empty_tiny_and_large_inputs_without_overflow() {
        let empty = plan(&[]);
        assert!(empty.leaves.is_empty());
        assert!(empty.limits.is_empty());
        assert_eq!(empty.allocated_bytes, 0);
        let scope = user(1000, "app", "term.scope", Role::Terminal, u64::MAX);
        let tiny = plan_protection(
            Path::new(ROOT),
            8 * PAGE,
            PAGE,
            std::slice::from_ref(&scope),
        )
        .unwrap();
        assert_eq!(tiny.budget_bytes, PAGE);
        assert_eq!(tiny.allocated_bytes, PAGE);
        let huge = plan_protection(Path::new(ROOT), u64::MAX, 65536, &[scope]).unwrap();
        assert_eq!(huge.budget_bytes, 2 * GIB);
        assert_eq!(huge.allocated_bytes, 128 * MIB);
    }

    #[test]
    fn protection_plan_can_target_an_independent_fixture_root() {
        let root = Path::new("/sys/fs/cgroup/zmem-protection-fixture-42");
        let scope = Scope {
            path: root.join("user.slice/user-1000.slice/user@1000.service/app.slice/term.scope"),
            role: Role::Terminal,
            current_bytes: 16 * MIB,
        };
        let p = plan_protection(root, 16 * GIB, PAGE, &[scope]).unwrap();
        assert!(p.limits.keys().all(|path| path.starts_with(root)));
        assert!(!p.limits.contains_key(&root.join("memory.low")));
    }
}
