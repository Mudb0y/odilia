#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, serde::Serialize, serde::Deserialize)]
pub enum ElementType {
	Heading,
	HeadingLevel1,
	HeadingLevel2,
	HeadingLevel3,
	HeadingLevel4,
	HeadingLevel5,
	HeadingLevel6,
	Button,
	Text,
	Table,
	TableCell,
	List,
	ListItem,
	Video,
	Audio,
	Link,
	Tab, // This is when you're looking at tabs in a dialog
}
